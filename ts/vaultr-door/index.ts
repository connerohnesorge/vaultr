// Door library — a door is a Plant job (door-<name>.<interval>.ts) that watches
// ingested Vault Content and launches one Herdr agent session per batch of new
// files, via `plant agent run` (never a bare CLI). The library owns the fragile
// parts once: ordered durable claim, ingestion-root allowlist, rolling-window
// fire breaker, typed idempotent launch. Spec: .rocks/specs/vault-doors/spec.md.

import { createHash, randomUUID } from "node:crypto";
import {
  closeSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { basename, relative, resolve } from "node:path";

const HOME = process.env.HOME ?? "";
const vaultRoot = () => process.env.DOOR_VAULT_ROOT ?? `${HOME}/.dotfiles/vault`;
const stateDir = () => process.env.DOOR_STATE_DIR ?? `${HOME}/.local/state/plant/doors`;
const plantBin = () => process.env.PLANT_BIN ?? "plant";
// Ingestion-only roots (written by sync jobs, never by agents) — a door cannot
// watch agent-written Vault Content, so it cannot trigger off its own output.
const allowedRoots = () => (process.env.DOOR_ROOTS ?? "mail,teams,tickets").split(",").map((r) => r.trim()).filter(Boolean);

const WINDOW_MS = 3_600_000;

export type AgentOutcome = "Succeeded" | "Unavailable" | "Failed";

export interface AgentOpts {
  cli?: "claude" | "codex";
  label?: string;
  model?: string;
  args?: string;
  cwd?: string;
  timeout?: string; // plant duration, e.g. "45m"
  cleanup?: "never" | "always" | "on-success";
  idempotencyKey?: string;
}

/** Typed client over `plant agent run` — the only sanctioned way to drive an
 *  agent pane. Exit contract mirrors plant: 0 Succeeded, 75 Unavailable, else Failed. */
export async function agentRun(prompt: string, opts: AgentOpts = {}): Promise<{ outcome: AgentOutcome; detail: string }> {
  const argv = [plantBin(), "agent", "run", "--cli", opts.cli ?? "claude", "--label", opts.label ?? "agent"];
  for (const [flag, v] of [["--model", opts.model], ["--args", opts.args], ["--cwd", opts.cwd], ["--timeout", opts.timeout], ["--cleanup", opts.cleanup], ["--idempotency-key", opts.idempotencyKey]] as const) {
    if (v) argv.push(flag, v);
  }
  const proc = Bun.spawn(argv, { stdin: new TextEncoder().encode(prompt), stdout: "pipe", stderr: "pipe" });
  const [out, err, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  const lastLine = (s: string) => s.split("\n").map((l) => l.trim()).filter(Boolean).pop();
  const detail = lastLine(out) ?? lastLine(err) ?? "no output";
  const outcome: AgentOutcome = code === 0 ? "Succeeded" : code === 75 ? "Unavailable" : "Failed";
  return { outcome, detail };
}

export interface DoorFile {
  path: string; // vault-root-relative
  abs: string;
  mtimeMs: number;
  text: string; // file content (first 64 KiB), for content filters and prompts
}

export interface DoorSpec {
  /** Defaults to the job filename: door-oncall.30m.ts -> "oncall". */
  name?: string;
  /** Glob relative to the vault content root; first segment must be an allowlisted ingestion root. */
  watch: string;
  filter?: (f: DoorFile) => boolean;
  prompt: (files: DoorFile[]) => string;
  agent?: AgentOpts;
  /** Rolling-window breaker; exceeding this pauses the door until manual re-arm. */
  maxFiresPerHour?: number;
}

export interface DoorResult {
  code: 0 | 1 | 75; // Plant job exit contract: 0 success, 75 retry-no-record, else failed
  detail: string;
}

interface Cursor {
  mtimeMs: number;
  path: string;
}

type ClaimFile = Cursor;

interface DoorClaim {
  from: Cursor;
  files: ClaimFile[];
  key: string;
}

interface DoorState {
  version: 1;
  cursor: Cursor;
  fires: number[]; // epoch-ms of launches in/near the rolling window
  paused?: string; // reason; presence = paused until manually removed
  claim?: DoorClaim;
}

function statePath(name: string): string {
  return `${stateDir()}/${name}.json`;
}

function lockPath(name: string): string {
  return `${stateDir()}/${name}.lock`;
}

function compareCursor(a: Cursor, b: Cursor): number {
  if (a.mtimeMs !== b.mtimeMs) return a.mtimeMs < b.mtimeMs ? -1 : 1;
  return a.path < b.path ? -1 : a.path > b.path ? 1 : 0;
}

function sameCursor(a: Cursor, b: Cursor): boolean {
  return a.mtimeMs === b.mtimeMs && a.path === b.path;
}

function validCursor(value: unknown): value is Cursor {
  const cursor = value as Cursor;
  return !!cursor
    && Number.isFinite(cursor.mtimeMs)
    && cursor.mtimeMs >= 0
    && typeof cursor.path === "string";
}

function claimKey(name: string, from: Cursor, files: ClaimFile[]): string {
  return createHash("sha256")
    .update(JSON.stringify({ door: name, from, files }))
    .digest("hex");
}

function parseState(name: string, value: unknown): DoorState {
  const raw = value as Record<string, unknown>;
  if (!raw || typeof raw !== "object" || !Array.isArray(raw.fires)
    || !raw.fires.every((fire) => Number.isFinite(fire) && (fire as number) >= 0)
    || (raw.paused !== undefined && typeof raw.paused !== "string")) {
    throw new Error("invalid door state");
  }

  // Shipped scalar states are migrated conservatively: every path tied at the
  // old high-water timestamp is treated as already fired, never duplicated.
  if (raw.cursor !== undefined && raw.version !== 1) {
    throw new Error("unsupported door state version");
  }
  const cursor = validCursor(raw.cursor)
    ? raw.cursor
    : Number.isFinite(raw.hwm) && (raw.hwm as number) >= 0
      ? { mtimeMs: raw.hwm as number, path: "\uffff" }
      : undefined;
  if (!cursor) throw new Error("invalid door cursor");

  const state: DoorState = {
    version: 1,
    cursor,
    fires: raw.fires as number[],
    ...(raw.paused === undefined ? {} : { paused: raw.paused as string }),
  };
  if (raw.claim !== undefined) {
    const claim = raw.claim as DoorClaim;
    if (!claim || !validCursor(claim.from) || !sameCursor(claim.from, cursor)
      || !Array.isArray(claim.files) || claim.files.length === 0
      || !claim.files.every(validCursor)
      || claim.files.some((file) => file.path.length === 0)
      || claim.files.some((file, i) =>
        compareCursor(file, claim.from) <= 0
        || (i > 0 && compareCursor(claim.files[i - 1]!, file) >= 0))
      || typeof claim.key !== "string"
      || claim.key !== claimKey(name, claim.from, claim.files)) {
      throw new Error("invalid door claim");
    }
    state.claim = claim;
  }
  return state;
}

function loadState(name: string): DoorState {
  try {
    return parseState(name, JSON.parse(readFileSync(statePath(name), "utf8")));
  } catch (error: any) {
    if (error?.code === "ENOENT") {
      return { version: 1, cursor: { mtimeMs: 0, path: "" }, fires: [] };
    }
    throw new Error(`cannot read ${statePath(name)}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function syncDirectory(path: string): void {
  const fd = openSync(path, "r");
  try {
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
}

function saveState(name: string, state: DoorState): void {
  mkdirSync(stateDir(), { recursive: true });
  const path = statePath(name);
  const tmp = `${path}.tmp-${randomUUID()}`;
  let fd: number | undefined;
  try {
    fd = openSync(tmp, "wx", 0o600);
    writeSync(fd, `${JSON.stringify(state)}\n`);
    fsyncSync(fd);
    closeSync(fd);
    fd = undefined;
    renameSync(tmp, path);
    syncDirectory(stateDir());
  } catch (error) {
    if (fd !== undefined) closeSync(fd);
    try {
      unlinkSync(tmp);
    } catch {}
    throw error;
  }
}

function lockOwner(path: string): { pid: number; token: string } {
  const owner = JSON.parse(readFileSync(path, "utf8"));
  if (!Number.isInteger(owner?.pid) || owner.pid <= 0 || typeof owner?.token !== "string") {
    throw new Error(`invalid door lock ${path}`);
  }
  return owner;
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error: any) {
    return error?.code === "EPERM";
  }
}

function acquireLock(name: string): (() => void) | undefined {
  mkdirSync(stateDir(), { recursive: true });
  const path = lockPath(name);
  for (let attempt = 0; attempt < 3; attempt++) {
    const token = randomUUID();
    let fd: number;
    try {
      fd = openSync(path, "wx", 0o600);
    } catch (error: any) {
      if (error?.code !== "EEXIST") throw error;
      const owner = lockOwner(path);
      if (pidAlive(owner.pid)) return undefined;
      try {
        unlinkSync(path);
        syncDirectory(stateDir());
      } catch (unlinkError: any) {
        if (unlinkError?.code !== "ENOENT") throw unlinkError;
      }
      continue;
    }
    try {
      writeSync(fd, `${JSON.stringify({ pid: process.pid, token })}\n`);
      fsyncSync(fd);
      closeSync(fd);
      syncDirectory(stateDir());
    } catch (error) {
      try {
        closeSync(fd);
      } catch {}
      try {
        unlinkSync(path);
        syncDirectory(stateDir());
      } catch {}
      throw error;
    }
    return () => {
      if (lockOwner(path).token !== token) {
        throw new Error(`door lock ownership changed for ${name}`);
      }
      unlinkSync(path);
      syncDirectory(stateDir());
    };
  }
  throw new Error(`could not recover stale door lock ${path}`);
}

function absoluteClaimPath(path: string): string {
  const root = resolve(vaultRoot());
  const absolute = resolve(root, path);
  const rel = relative(root, absolute);
  if (rel === ".." || rel.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`)) {
    throw new Error(`claimed path escapes vault root: ${path}`);
  }
  return absolute;
}

function hydrateClaim(claim: DoorClaim): DoorFile[] {
  return claim.files.map((file) => {
    const abs = absoluteClaimPath(file.path);
    const stat = statSync(abs);
    if (stat.mtimeMs !== file.mtimeMs) {
      throw new Error(`claimed file changed before launch: ${file.path}`);
    }
    return {
      ...file,
      abs,
      text: readFileSync(abs, "utf8").slice(0, 65536),
    };
  });
}

function doorNameFromScript(): string {
  // "door-oncall.30m.ts" -> "oncall"
  const first = basename(Bun.main).split(".")[0] ?? "door";
  return first.startsWith("door-") ? first.slice(5) : first;
}

export async function door(spec: DoorSpec): Promise<DoorResult> {
  const name = spec.name ?? doorNameFromScript();
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name)) {
    return { code: 1, detail: `invalid door name "${name}"` };
  }
  let release: (() => void) | undefined;
  try {
    release = acquireLock(name);
    if (!release) return { code: 75, detail: `door "${name}" is already running` };
    const state = loadState(name);

    if (state.paused) {
      return { code: 0, detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath(name)}` };
    }

    const root = spec.watch.split("/")[0] ?? "";
    if (/[*?[\]]/.test(root) || !allowedRoots().includes(root)) {
      return { code: 1, detail: `watch "${spec.watch}" rejected: first segment must be an ingestion root (${allowedRoots().join(", ")})` };
    }

    if (!state.claim) {
      const files: DoorFile[] = [];
      for (const path of new Bun.Glob(spec.watch).scanSync({ cwd: vaultRoot() })) {
        const abs = absoluteClaimPath(path);
        let mtimeMs: number;
        try {
          mtimeMs = statSync(abs).mtimeMs;
        } catch {
          continue; // raced away mid-scan
        }
        if (compareCursor({ mtimeMs, path }, state.cursor) > 0) {
          const text = readFileSync(abs, "utf8").slice(0, 65536);
          files.push({ path, abs, mtimeMs, text });
        }
      }
      files.sort(compareCursor);
      const matched = spec.filter ? files.filter(spec.filter) : files;
      if (matched.length === 0) return { code: 0, detail: "no new files" };

      const now = Date.now();
      state.fires = state.fires.filter((time) => now - time < WINDOW_MS);
      const max = spec.maxFiresPerHour ?? 4;
      if (state.fires.length >= max) {
        state.paused = `fire limit hit (${state.fires.length} fires in 1h, max ${max})`;
        saveState(name, state);
        return { code: 1, detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath(name)}` };
      }

      const claimFiles = matched.map(({ mtimeMs, path }) => ({ mtimeMs, path }));
      state.claim = {
        from: state.cursor,
        files: claimFiles,
        key: claimKey(name, state.cursor, claimFiles),
      };
      saveState(name, state);
    }

    const claim = state.claim;
    const matched = hydrateClaim(claim);
    const { outcome, detail } = await agentRun(spec.prompt(matched), {
      label: `door-${name}`,
      ...spec.agent,
      idempotencyKey: claim.key,
    });
    if (outcome === "Unavailable") {
      return { code: 75, detail: `herdr unavailable, ${matched.length} file(s) held` };
    }

    state.fires.push(Date.now());
    state.cursor = claim.files[claim.files.length - 1]!;
    delete state.claim;
    saveState(name, state);
    return outcome === "Succeeded"
      ? { code: 0, detail: `fired on ${matched.length} file(s): ${detail}` }
      : { code: 1, detail: `agent failed after claiming ${matched.length} file(s): ${detail}` };
  } catch (error) {
    return {
      code: 1,
      detail: `door "${name}" failed closed: ${error instanceof Error ? error.message : String(error)}`,
    };
  } finally {
    release?.();
  }
}

/** Entry point for door job scripts: print the outcome and exit with the Plant job code. */
export async function runDoor(spec: DoorSpec): Promise<never> {
  const { code, detail } = await door(spec);
  console.log(detail);
  process.exit(code);
}
