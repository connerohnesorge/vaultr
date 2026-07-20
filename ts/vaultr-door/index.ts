// A Door is a Plant job that turns new ingestion files into one durable agent run.

import { basename } from "node:path";
import { agentRun, receiptDurable } from "./agent-run.ts";
import type { AgentOpts } from "./agent-run.ts";
import {
  claimRelativePath,
  hiddenSegmentsAllowed,
  isExplicitHiddenLiteral,
  loadIngestionFile,
  resolveIngestionRoot,
  type IngestionRoot,
} from "./ingestion.ts";
import { InitialFileMissingError } from "./safe-loader.ts";
import {
  acquireLock,
  advanceFrontier,
  claimKey,
  compareFile,
  isNew,
  loadState,
  saveState,
  validClaimFile,
  type DoorClaim,
  type DoorState,
} from "./state.ts";

export { agentRun } from "./agent-run.ts";
export type { AgentOpts, AgentRunReceipt } from "./agent-run.ts";

const WINDOW_MS = 3_600_000;

export interface DoorFile {
  path: string;
  abs: string;
  mtimeMs: number;
  size: number;
  sha256: string;
  text: string;
}

export interface DoorSpec {
  /** Defaults to the job filename: door-oncall.30m.ts -> "oncall". */
  name?: string;
  /** Traversal-free glob beneath one canonical allowlisted ingestion root. */
  watch: string;
  filter?: (file: DoorFile) => boolean;
  prompt: (files: DoorFile[]) => string;
  agent?: AgentOpts;
  /** Rolling-window breaker; exceeding this pauses the door until manual re-arm. */
  maxFiresPerHour?: number;
}

export interface DoorResult {
  code: 0 | 1 | 75;
  detail: string;
}

function hydrateClaim(root: IngestionRoot, claim: DoorClaim): DoorFile[] {
  if (!claim.files.every(validClaimFile)) {
    throw new Error("legacy claim content identity was not bound");
  }
  return claim.files.map((file) => {
    const loaded = loadIngestionFile(
      root,
      claimRelativePath(root, file.path),
    );
    if (!loaded
      || loaded.mtimeMs !== file.mtimeMs
      || loaded.size !== file.size
      || loaded.sha256 !== file.sha256) {
      throw new Error(`claimed file changed before launch: ${file.path}`);
    }
    return {
      ...file,
      abs: loaded.abs,
      text: loaded.text,
    };
  });
}

function bindLegacyClaimIdentity(
  name: string,
  root: IngestionRoot,
  state: DoorState,
  save: () => void,
): void {
  const claim = state.claim;
  if (!claim?.legacyCursor || claim.files.every(validClaimFile)) return;
  claim.files = claim.files.map((file) => {
    const loaded = loadIngestionFile(
      root,
      claimRelativePath(root, file.path),
    );
    if (!loaded || loaded.mtimeMs !== file.mtimeMs) {
      throw new Error(
        `legacy claimed file changed before identity binding: ${file.path}`,
      );
    }
    return {
      mtimeMs: file.mtimeMs,
      path: file.path,
      size: loaded.size,
      sha256: loaded.sha256,
    };
  });
  save();
}

function doorNameFromScript(): string {
  const first = basename(Bun.main).split(".")[0] ?? "door";
  return first.startsWith("door-") ? first.slice(5) : first;
}

function scan(
  root: IngestionRoot,
  state: DoorState,
  filter?: (file: DoorFile) => boolean,
): DoorFile[] {
  const files: DoorFile[] = [];
  for (const path of new Bun.Glob(root.pattern).scanSync({
    cwd: root.path,
    dot: root.pattern.split("/").some(isExplicitHiddenLiteral),
    followSymlinks: false,
    onlyFiles: false,
  })) {
    if (!hiddenSegmentsAllowed(root.pattern, path)) continue;
    try {
      const vaultPath = `${root.prefix}/${path}`;
      const loaded = loadIngestionFile(
        root,
        path,
        (mtimeMs) => isNew({ mtimeMs, path: vaultPath }, state.frontier),
      );
      if (!loaded) continue;
      files.push({
        path: vaultPath,
        abs: loaded.abs,
        mtimeMs: loaded.mtimeMs,
        size: loaded.size,
        sha256: loaded.sha256,
        text: loaded.text,
      });
    } catch (error) {
      if (error instanceof InitialFileMissingError) continue;
      throw error;
    }
  }
  files.sort(compareFile);
  return filter ? files.filter(filter) : files;
}

export async function door(spec: DoorSpec): Promise<DoorResult> {
  const name = spec.name ?? doorNameFromScript();
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name)) {
    return { code: 1, detail: `invalid door name "${name}"` };
  }
  let lease: Awaited<ReturnType<typeof acquireLock>>;
  try {
    lease = await acquireLock(name);
    if (!lease) return { code: 75, detail: `door "${name}" is already running` };
    const { store } = lease;
    const state = loadState(store, name);
    const root = resolveIngestionRoot(spec.watch);
    const statePath = store.displayPath(`${name}.json`);
    const persist = () => saveState(store, name, state);

    if (state.paused) {
      return {
        code: 0,
        detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath}`,
      };
    }

    if (!state.claim) {
      const matched = scan(root, state, spec.filter);
      if (matched.length === 0) return { code: 0, detail: "no new files" };

      const now = Date.now();
      state.fires = state.fires.filter((time) => now - time < WINDOW_MS);
      const max = spec.maxFiresPerHour ?? 4;
      if (state.fires.length >= max) {
        state.paused = `fire limit hit (${state.fires.length} fires in 1h, max ${max})`;
        persist();
        return {
          code: 1,
          detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath}`,
        };
      }

      const claimFiles = matched.map(({ mtimeMs, path, size, sha256 }) => ({
        mtimeMs,
        path,
        size,
        sha256,
      }));
      state.claim = {
        from: state.frontier,
        files: claimFiles,
        key: claimKey(name, state.frontier, claimFiles),
      };
      persist();
    }

    bindLegacyClaimIdentity(name, root, state, persist);
    const claim = state.claim!;
    const matched = hydrateClaim(root, claim);
    const result = await agentRun(spec.prompt(matched), {
      label: `door-${name}`,
      ...spec.agent,
      idempotencyKey: claim.key,
    });
    if (!receiptDurable(result)) {
      return result.outcome === "retryable"
        ? {
          code: 75,
          detail: `agent retryable, ${matched.length} file(s) held: ${result.detail}`,
        }
        : {
          code: 1,
          detail: `agent outcome indeterminate, ${matched.length} file(s) held: ${result.detail}`,
        };
    }

    state.fires.push(Date.now());
    state.frontier = advanceFrontier(claim.from, claim.files);
    delete state.claim;
    persist();
    return result.outcome === "succeeded"
      ? {
        code: 0,
        detail: `fired on ${matched.length} file(s): ${result.detail}`,
      }
      : {
        code: 1,
        detail: `agent failed after claiming ${matched.length} file(s): ${result.detail}`,
      };
  } catch (error) {
    return {
      code: 1,
      detail: `door "${name}" failed closed: ${
        error instanceof Error ? error.message : String(error)
      }`,
    };
  } finally {
    lease?.release();
  }
}

/** Print the outcome and exit with Plant's job exit contract. */
export async function runDoor(spec: DoorSpec): Promise<never> {
  const { code, detail } = await door(spec);
  console.log(detail);
  process.exit(code);
}
