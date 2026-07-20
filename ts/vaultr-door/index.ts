// Door library — a door is a Plant job (door-<name>.<interval>.ts) that watches
// ingested Vault Content and launches one Herdr agent session per batch of new
// files, via `plant agent run` (never a bare CLI). The library owns the fragile
// parts once: ordered durable claim, ingestion-root allowlist, rolling-window
// fire breaker, typed idempotent launch. Spec: .rocks/specs/vault-doors/spec.md.

import { createHash, randomUUID } from "node:crypto";
import { dlopen, FFIType } from "bun:ffi";
import {
  closeSync,
  constants,
  fsyncSync,
  linkSync,
  lstatSync,
  mkdirSync,
  openSync,
  readSync,
  realpathSync,
  renameSync,
  statSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { basename, dirname, isAbsolute, relative, resolve, sep } from "node:path";
import {
  canonicalDirectory,
  InitialFileMissingError,
  openStableRegularFileDescriptor,
  readStableRegularDescriptor,
  readStableRegularFile,
  StableFileIdentityError,
  type StableRegularFileDescriptor,
  validRelativeFilePath,
  withStableRegularFile,
} from "./safe-loader.ts";

const HOME = process.env.HOME ?? "";
const vaultRoot = () => process.env.DOOR_VAULT_ROOT ?? `${HOME}/.dotfiles/vault`;
const stateDir = () => process.env.DOOR_STATE_DIR ?? `${HOME}/.local/state/plant/doors`;
const plantBin = () => process.env.PLANT_BIN ?? "plant";
// Ingestion-only roots (written by sync jobs, never by agents) — a door cannot
// watch agent-written Vault Content, so it cannot trigger off its own output.
const allowedRoots = () => (process.env.DOOR_ROOTS ?? "mail,teams,tickets").split(",").map((r) => r.trim()).filter(Boolean);

const WINDOW_MS = 3_600_000;
const MAX_STATE_BYTES = 1_048_576;
const LOCK_EX = 2;
const LOCK_NB = 4;
const LOCK_UN = 8;
const kernelLock = (() => {
  const library = process.platform === "darwin"
    ? "/usr/lib/libSystem.B.dylib"
    : process.platform === "linux" ? "libc.so.6" : undefined;
  if (!library) return undefined;
  try {
    return dlopen(library, {
      flock: { args: [FFIType.i32, FFIType.i32], returns: FFIType.i32 },
    });
  } catch {
    return undefined;
  }
})();

let afterIngestionStatForTest: ((path: string) => void) | undefined;
let afterMetadataStatForTest: ((path: string) => void) | undefined;
let beforeStatePublishForTest: (() => void) | undefined;
let beforeStaleRecoveryForTest:
  | ((owner: { pid: number; token: string }) => void | Promise<void>)
  | undefined;
let afterStaleOwnerReadForTest:
  | ((owner: { pid: number; token: string }) => void | Promise<void>)
  | undefined;

export function setIngestionStatHookForTest(hook?: (path: string) => void): void {
  afterIngestionStatForTest = hook;
}

export function setMetadataStatHookForTest(hook?: (path: string) => void): void {
  afterMetadataStatForTest = hook;
}

export function setStatePublishHookForTest(hook?: () => void): void {
  beforeStatePublishForTest = hook;
}

export function setStaleRecoveryHookForTest(
  hook?: (owner: { pid: number; token: string }) => void | Promise<void>,
): void {
  beforeStaleRecoveryForTest = hook;
}

export function setStaleOwnerReadHookForTest(
  hook?: (owner: { pid: number; token: string }) => void | Promise<void>,
): void {
  afterStaleOwnerReadForTest = hook;
}

export type AgentRunReceipt =
  | { outcome: "succeeded"; detail: string }
  | { outcome: "failed"; detail: string }
  | { outcome: "untracked_succeeded"; detail: string }
  | { outcome: "untracked_failed"; detail: string }
  | { outcome: "retryable"; detail: string }
  | { outcome: "indeterminate"; detail: string };

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
 * agent pane. A conclusive result is durable only when Plant says its stable
 * idempotency record was committed. */
function receiptExitCode(receipt: AgentRunReceipt): 0 | 1 | 75 {
  if (receipt.outcome === "succeeded" || receipt.outcome === "untracked_succeeded") return 0;
  return receipt.outcome === "retryable" ? 75 : 1;
}

function receiptDurable(receipt: AgentRunReceipt): boolean {
  return receipt.outcome === "succeeded" || receipt.outcome === "failed";
}

function parseReceipt(value: unknown): AgentRunReceipt | undefined {
  const receipt = value as Record<string, unknown>;
  if (!receipt || typeof receipt !== "object" || Array.isArray(receipt)
    || Object.keys(receipt).some((key) => key !== "outcome" && key !== "detail")
    || typeof receipt.detail !== "string"
    || !["succeeded", "failed", "untracked_succeeded", "untracked_failed", "retryable", "indeterminate"].includes(receipt.outcome as string)) {
    return undefined;
  }
  return receipt as AgentRunReceipt;
}

export async function agentRun(prompt: string, opts: AgentOpts = {}): Promise<AgentRunReceipt> {
  const argv = [plantBin(), "agent", "run", "--cli", opts.cli ?? "claude", "--label", opts.label ?? "agent"];
  for (const [flag, v] of [["--model", opts.model], ["--args", opts.args], ["--cwd", opts.cwd], ["--timeout", opts.timeout], ["--cleanup", opts.cleanup], ["--idempotency-key", opts.idempotencyKey]] as const) {
    if (v) argv.push(flag, v);
  }
  const proc = Bun.spawn(argv, { stdin: new TextEncoder().encode(prompt), stdout: "pipe", stderr: "pipe" });
  const [out, err, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  const lastLine = (s: string) => s.split("\n").map((l) => l.trim()).filter(Boolean).pop();
  const fallback = lastLine(err) ?? lastLine(out) ?? "no output";
  let receipt: AgentRunReceipt | undefined;
  try {
    receipt = parseReceipt(JSON.parse(lastLine(out) ?? ""));
  } catch {}
  if (!receipt || receiptExitCode(receipt) !== code) {
    return { outcome: "indeterminate", detail: `invalid Plant receipt (exit ${code}): ${fallback}` };
  }
  return receipt;
}

export interface DoorFile {
  path: string; // vault-root-relative
  abs: string;
  mtimeMs: number;
  size: number;
  sha256: string;
  text: string; // file content (first 64 KiB), for content filters and prompts
}

export interface DoorSpec {
  /** Defaults to the job filename: door-oncall.30m.ts -> "oncall". */
  name?: string;
  /** Traversal-free glob beneath one canonical allowlisted ingestion root. */
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

interface FileOrder {
  mtimeMs: number;
  path: string;
}

interface ClaimFile extends FileOrder {
  size: number;
  sha256: string;
}

type LegacyClaimFile = FileOrder;

interface Frontier {
  mtimeMs: number;
  seen: string[];
  closedThroughPath?: string;
}

interface LegacyCursor {
  mtimeMs: number;
  path: string;
}

interface DoorClaim {
  from: Frontier;
  files: Array<ClaimFile | LegacyClaimFile>;
  key: string;
  legacyCursor?: LegacyCursor;
}

interface DoorState {
  version: 2;
  frontier: Frontier;
  fires: number[]; // epoch-ms of launches in/near the rolling window
  paused?: string; // reason; presence = paused until manually removed
  claim?: DoorClaim;
}

function statePath(name: string): string {
  return `${stateDir()}/${name}.json`;
}

function compareFile(a: FileOrder, b: FileOrder): number {
  if (a.mtimeMs !== b.mtimeMs) return a.mtimeMs < b.mtimeMs ? -1 : 1;
  return a.path < b.path ? -1 : a.path > b.path ? 1 : 0;
}

function sameFrontier(a: Frontier, b: Frontier): boolean {
  return a.mtimeMs === b.mtimeMs
    && a.closedThroughPath === b.closedThroughPath
    && a.seen.length === b.seen.length
    && a.seen.every((path, index) => path === b.seen[index]);
}

function validTimestamp(value: unknown): value is number {
  return typeof value === "number"
    && Number.isFinite(value)
    && value >= 0
    && !Object.is(value, -0);
}

function validFileOrder(value: unknown): value is FileOrder {
  const file = value as FileOrder;
  return !!file
    && validTimestamp(file.mtimeMs)
    && typeof file.path === "string"
    && file.path.length > 0;
}

function validLegacyClaimFile(value: unknown): value is LegacyClaimFile {
  const file = value as Record<string, unknown>;
  return validFileOrder(value)
    && file.size === undefined
    && file.sha256 === undefined;
}

function validClaimFile(value: unknown): value is ClaimFile {
  const file = value as ClaimFile;
  return validFileOrder(file)
    && Number.isSafeInteger(file.size)
    && file.size >= 0
    && typeof file.sha256 === "string"
    && /^[0-9a-f]{64}$/.test(file.sha256);
}

function validLegacyCursor(value: unknown): value is LegacyCursor {
  const cursor = value as LegacyCursor;
  return !!cursor
    && validTimestamp(cursor.mtimeMs)
    && typeof cursor.path === "string"
    && (cursor.path === ""
      ? cursor.mtimeMs === 0
      : validRelativePath(cursor.path, true));
}

function validFrontier(value: unknown): value is Frontier {
  const frontier = value as Frontier;
  return !!frontier
    && validTimestamp(frontier.mtimeMs)
    && Array.isArray(frontier.seen)
    && frontier.seen.every((path) => typeof path === "string" && path.length > 0)
    && frontier.seen.every((path) => validRelativePath(path, true))
    && frontier.seen.every((path, index) => index === 0 || frontier.seen[index - 1]! < path)
    && (frontier.closedThroughPath === undefined
      || (typeof frontier.closedThroughPath === "string"
        && (frontier.closedThroughPath === "" || validRelativePath(frontier.closedThroughPath, true))
        && frontier.seen.every((path) => path > frontier.closedThroughPath!)));
}

function isNew(file: FileOrder, frontier: Frontier): boolean {
  return file.mtimeMs > frontier.mtimeMs
    || (file.mtimeMs === frontier.mtimeMs
      && (frontier.closedThroughPath === undefined || file.path > frontier.closedThroughPath)
      && !frontier.seen.includes(file.path));
}

function claimKey(name: string, from: Frontier, files: ClaimFile[]): string {
  return createHash("sha256")
    .update(JSON.stringify({ door: name, from, files }))
    .digest("hex");
}

function legacyClaimKey(name: string, from: LegacyCursor, files: FileOrder[]): string {
  return createHash("sha256")
    .update(JSON.stringify({
      door: name,
      from,
      files: files.map(({ mtimeMs, path }) => ({ mtimeMs, path })),
    }))
    .digest("hex");
}

function conservativeFrontier(mtimeMs: number): Frontier {
  if (!validTimestamp(mtimeMs)) throw new Error("invalid legacy timestamp");
  const value = new Float64Array([mtimeMs]);
  const bits = new BigUint64Array(value.buffer);
  bits[0] = bits[0]! + 1n;
  if (!validTimestamp(value[0]) || value[0]! <= mtimeMs) {
    throw new Error("legacy timestamp cannot be closed");
  }
  return { mtimeMs: value[0]!, seen: [] };
}

function parseState(name: string, value: unknown): DoorState {
  const raw = value as Record<string, unknown>;
  if (!raw || typeof raw !== "object" || !Array.isArray(raw.fires)
    || !raw.fires.every(validTimestamp)
    || (raw.paused !== undefined && typeof raw.paused !== "string")
    || raw.version !== 2
    || !validFrontier(raw.frontier)) {
    throw new Error("invalid door state");
  }

  const state: DoorState = {
    version: 2,
    frontier: raw.frontier,
    fires: raw.fires as number[],
    ...(raw.paused === undefined ? {} : { paused: raw.paused as string }),
  };
  if (raw.claim !== undefined) {
    const claim = raw.claim as DoorClaim;
    const legacy = claim?.legacyCursor;
    const files = Array.isArray(claim?.files) ? claim.files : [];
    const identifiedFiles = files.length > 0 && files.every(validClaimFile);
    const unboundLegacyFiles = files.length > 0 && files.every(validLegacyClaimFile);
    if (!claim || !validFrontier(claim.from) || !sameFrontier(claim.from, state.frontier)
      || files.length === 0
      || (legacy ? !identifiedFiles && !unboundLegacyFiles : !identifiedFiles)
      || claim.files.some((file) => !validRelativePath(file.path, true))
      || new Set(claim.files.map((file) => file.path)).size !== claim.files.length
      || claim.files.some((file, i) =>
        (legacy ? compareFile(file, legacy) <= 0 : !isNew(file, claim.from))
        || (i > 0 && compareFile(claim.files[i - 1]!, file) >= 0))
      || typeof claim.key !== "string"
      || (legacy
        ? !validLegacyCursor(legacy)
          || claim.from.mtimeMs !== legacy.mtimeMs
          || claim.from.closedThroughPath !== legacy.path
          || claim.from.seen.length !== 0
          || claim.key !== legacyClaimKey(name, legacy, claim.files)
        : claim.key !== claimKey(name, claim.from, claim.files as ClaimFile[]))) {
      throw new Error("invalid door claim");
    }
    state.claim = claim;
  }
  return state;
}

function migrateLegacyState(name: string, value: unknown): DoorState {
  const raw = value as Record<string, unknown>;
  if (!raw || typeof raw !== "object"
    || !Array.isArray(raw.fires)
    || !raw.fires.every(validTimestamp)
    || (raw.paused !== undefined && typeof raw.paused !== "string")) {
    throw new Error("invalid legacy door state");
  }
  const metadata = {
    fires: raw.fires as number[],
    ...(raw.paused === undefined ? {} : { paused: raw.paused as string }),
  };
  if (raw.version === undefined && validTimestamp(raw.hwm)
    && raw.cursor === undefined && raw.claim === undefined) {
    return {
      version: 2,
      frontier: conservativeFrontier(raw.hwm as number),
      ...metadata,
    };
  }
  if (raw.version !== 1 || !validLegacyCursor(raw.cursor)) {
    throw new Error("unsupported door state version");
  }
  const cursor = raw.cursor;
  const state: DoorState = {
    version: 2,
    frontier: {
      mtimeMs: cursor.mtimeMs,
      seen: [],
      closedThroughPath: cursor.path,
    },
    ...metadata,
  };
  if (raw.claim !== undefined) {
    const claim = raw.claim as { from: LegacyCursor; files: LegacyClaimFile[]; key: string };
    if (!claim || !validLegacyCursor(claim.from)
      || compareFile(claim.from, cursor) !== 0
      || !Array.isArray(claim.files) || claim.files.length === 0
      || !claim.files.every(validLegacyClaimFile)
      || claim.files.some((file) => !validRelativePath(file.path, true))
      || new Set(claim.files.map((file) => file.path)).size !== claim.files.length
      || claim.files.some((file, index) =>
        compareFile(file, cursor) <= 0
        || (index > 0 && compareFile(claim.files[index - 1]!, file) >= 0))
      || typeof claim.key !== "string"
      || claim.key !== legacyClaimKey(name, cursor, claim.files)) {
      throw new Error("invalid legacy door claim");
    }
    state.claim = {
      from: state.frontier,
      files: claim.files,
      key: claim.key,
      legacyCursor: cursor,
    };
  }
  return state;
}

function loadState(name: string): DoorState {
  const bytes = readControlFile(`${name}.json`);
  if (bytes === undefined) {
    return { version: 2, frontier: { mtimeMs: 0, seen: [] }, fires: [] };
  }
  try {
    const value = JSON.parse(bytes.toString("utf8"));
    if ((value as Record<string, unknown>)?.version === 2) return parseState(name, value);
    const state = parseState(name, migrateLegacyState(name, value));
    saveState(name, state);
    return state;
  } catch (error) {
    // Only the initial descriptor open may establish a fresh state. Migration
    // parsing and publication errors, including ENOENT, retain the source bytes
    // and fail closed.
    throw new Error(`cannot read supported ${statePath(name)}: ${error instanceof Error ? error.message : String(error)}`);
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

function ensureDirectoryDurable(path: string): void {
  const missing: string[] = [];
  let cursor = resolve(path);
  while (true) {
    try {
      if (!lstatSync(cursor).isDirectory()) throw new Error(`${cursor} is not a real directory`);
      break;
    } catch (error: any) {
      if (error?.code !== "ENOENT") throw error;
      missing.push(cursor);
      const parent = dirname(cursor);
      if (parent === cursor) throw error;
      cursor = parent;
    }
  }
  for (const dir of missing.reverse()) {
    try {
      mkdirSync(dir, { mode: 0o700 });
    } catch (error: any) {
      if (error?.code !== "EEXIST" || !lstatSync(dir).isDirectory()) throw error;
    }
    syncDirectory(dir);
    syncDirectory(dirname(dir));
  }
}

function saveState(name: string, state: DoorState): void {
  ensureDirectoryDurable(stateDir());
  beforeStatePublishForTest?.();
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

class IncompleteLockOwner extends Error {}

function readControlFile(fileName: string): Buffer | undefined {
  const root = canonicalDirectory(stateDir());
  try {
    return readStableRegularFile(
      root,
      fileName,
      MAX_STATE_BYTES,
      afterMetadataStatForTest,
    ).value;
  } catch (error) {
    if (error instanceof InitialFileMissingError) return undefined;
    throw error;
  }
}

function parseLockOwner(bytes: Buffer, path: string): { pid: number; token: string } {
  let owner: any;
  try {
    owner = JSON.parse(bytes.toString("utf8"));
  } catch {
    throw new IncompleteLockOwner(`invalid door lock ${path}`);
  }
  if (!Number.isInteger(owner?.pid) || owner.pid <= 0
    || typeof owner?.token !== "string" || owner.token.length === 0) {
    throw new IncompleteLockOwner(`invalid door lock ${path}`);
  }
  return owner;
}

function lockOwner(path: string): { pid: number; token: string } | undefined {
  const root = canonicalDirectory(dirname(path));
  let bytes: Buffer;
  try {
    bytes = readStableRegularFile(
      root,
      basename(path),
      MAX_STATE_BYTES,
      afterMetadataStatForTest,
    ).value;
  } catch (error) {
    if (error instanceof InitialFileMissingError) return undefined;
    throw error;
  }
  return parseLockOwner(bytes, path);
}

async function readPublishedLockOwner(
  path: string,
): Promise<{ pid: number; token: string } | undefined> {
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      return lockOwner(path);
    } catch (error: any) {
      if (!(error instanceof IncompleteLockOwner)) throw error;
      if (attempt < 3) await Bun.sleep(10);
    }
  }
  throw new Error(
    `incomplete legacy door lock ${path}; refusing automatic removal `
    + "until an offline migration verifies no legacy Door process exists",
  );
}

async function recoverStaleLock(
  path: string,
  observed: { pid: number; token: string },
): Promise<boolean> {
  if (!kernelLock) {
    throw new Error(`kernel-backed recovery unavailable; refusing to remove stale lock ${path}`);
  }
  const relativePath = basename(path);
  const root = canonicalDirectory(dirname(path));
  const canonicalPath = resolve(root, relativePath);
  let handle: StableRegularFileDescriptor;
  try {
    handle = openStableRegularFileDescriptor(root, relativePath, true);
  } catch (error) {
    if (error instanceof InitialFileMissingError) return false;
    throw error;
  }
  let locked = false;
  try {
    // Both contenders retain the canonical stale inode before they serialize.
    // A delayed contender can therefore prove the pathname was unlinked or now
    // names a successor instead of ever touching that successor.
    await beforeStaleRecoveryForTest?.(observed);
    for (let attempt = 0; attempt < 400; attempt++) {
      if (kernelLock.symbols.flock(handle.fd, LOCK_EX | LOCK_NB) === 0) {
        locked = true;
        break;
      }
      await Bun.sleep(5);
    }
    if (!locked) {
      throw new Error(`timed out serializing stale lock recovery for ${path}`);
    }

    let current: { pid: number; token: string };
    try {
      current = parseLockOwner(
        readStableRegularDescriptor(handle, relativePath, MAX_STATE_BYTES),
        path,
      );
    } catch (error) {
      if (error instanceof StableFileIdentityError) return false;
      throw error;
    }
    if (current.pid !== observed.pid || current.token !== observed.token || pidAlive(current.pid)) {
      return false;
    }
    await afterStaleOwnerReadForTest?.(current);
    try {
      const rechecked = parseLockOwner(
        readStableRegularDescriptor(handle, relativePath, MAX_STATE_BYTES),
        path,
      );
      if (rechecked.pid !== observed.pid
        || rechecked.token !== observed.token
        || pidAlive(rechecked.pid)) {
        return false;
      }
      handle.verify();
    } catch (error) {
      if (error instanceof StableFileIdentityError) return false;
      throw error;
    }
    unlinkSync(canonicalPath);
    syncDirectory(root);
    return true;
  } finally {
    if (locked) kernelLock.symbols.flock(handle.fd, LOCK_UN);
    handle.close();
  }
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error: any) {
    return error?.code === "EPERM";
  }
}

function publishLock(path: string, owner: { pid: number; token: string }): boolean {
  const root = dirname(path);
  const tmp = `${path}.tmp-${owner.token}`;
  let fd: number | undefined;
  let linked = false;
  try {
    fd = openSync(tmp, "wx", 0o600);
    writeSync(fd, `${JSON.stringify(owner)}\n`);
    fsyncSync(fd);
    closeSync(fd);
    fd = undefined;
    try {
      linkSync(tmp, path);
      linked = true;
    } catch (error: any) {
      if (error?.code === "EEXIST") return false;
      throw error;
    }
    syncDirectory(root);
    return true;
  } catch (error) {
    if (linked) {
      try {
        unlinkSync(path);
        syncDirectory(root);
      } catch {}
    }
    throw error;
  } finally {
    if (fd !== undefined) closeSync(fd);
    try {
      unlinkSync(tmp);
      syncDirectory(root);
    } catch {}
  }
}

async function acquireLock(name: string): Promise<(() => void) | undefined> {
  ensureDirectoryDurable(stateDir());
  const root = canonicalDirectory(stateDir());
  const path = resolve(root, `${name}.lock`);
  for (let attempt = 0; attempt < 3; attempt++) {
    if (canonicalDirectory(stateDir()) !== root) {
      throw new Error(`door state root changed while acquiring ${name}`);
    }
    const token = randomUUID();
    if (!publishLock(path, { pid: process.pid, token })) {
      const owner = await readPublishedLockOwner(path);
      if (!owner) continue;
      if (pidAlive(owner.pid)) return undefined;
      await recoverStaleLock(path, owner);
      continue;
    }
    return () => {
      if (lockOwner(path)?.token !== token) {
        throw new Error(`door lock ownership changed for ${name}`);
      }
      unlinkSync(path);
      syncDirectory(root);
    };
  }
  throw new Error(`could not recover stale door lock ${path}`);
}

interface IngestionRoot {
  prefix: string;
  path: string;
  pattern: string;
}

function isBeneath(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (!isAbsolute(rel) && rel !== ".." && !rel.startsWith(`..${sep}`));
}

function validRelativePath(path: string, allowGlob: boolean): boolean {
  if (!path || isAbsolute(path) || path.includes("\\") || path.includes("\0")) return false;
  const segments = path.split("/");
  return segments.every((segment) =>
    segment.length > 0
    && segment !== "."
    && segment !== ".."
    && (allowGlob || !/[*?[\]{}]/.test(segment)));
}

function isExplicitHiddenLiteral(segment: string): boolean {
  return segment.startsWith(".") && !/[*?[\]{}]/.test(segment);
}

function hiddenSegmentsAllowed(pattern: string, path: string): boolean {
  const expected = pattern.split("/").filter(isExplicitHiddenLiteral);
  const actual = path.split("/").filter((segment) => segment.startsWith("."));
  return expected.length === actual.length
    && expected.every((segment, index) => segment === actual[index]);
}

/** Resolve one trusted ingestion root and the watch pattern beneath it. Every
 * later path is resolved through this same canonical root. */
function resolveIngestionRoot(watch: string): IngestionRoot {
  if (!validRelativePath(watch, true)) {
    throw new Error(`watch "${watch}" rejected: must be a traversal-free relative glob`);
  }
  const configuredRoots = allowedRoots();
  const roots = configuredRoots
    .filter((root) => validRelativePath(root, false))
    .sort((a, b) => b.length - a.length);
  if (roots.length !== configuredRoots.length) {
    throw new Error("DOOR_ROOTS contains an invalid ingestion root");
  }
  const prefix = roots.find((root) => watch === root || watch.startsWith(`${root}/`));
  if (!prefix) {
    throw new Error(`watch "${watch}" rejected: must be beneath an ingestion root (${roots.join(", ")})`);
  }
  const vault = realpathSync(vaultRoot());
  const path = realpathSync(resolve(vault, prefix));
  if (!isBeneath(vault, path) || !statSync(path).isDirectory()) {
    throw new Error(`ingestion root "${prefix}" escapes the vault or is not a directory`);
  }
  return { prefix, path, pattern: watch === prefix ? "*" : watch.slice(prefix.length + 1) };
}

interface LoadedIngestionFile {
  abs: string;
  mtimeMs: number;
  size: number;
  sha256: string;
  text: string;
}

function loadIngestionFile(
  root: IngestionRoot,
  path: string,
  shouldRead?: (mtimeMs: number) => boolean,
): LoadedIngestionFile | undefined {
  if (!validRelativeFilePath(path)) {
    throw new Error(`invalid path beneath ingestion root: ${path}`);
  }
  const loaded = withStableRegularFile(
    root.path,
    path,
    {},
    (fd, before) => {
      if (shouldRead && !shouldRead(before.mtimeMs)) return undefined;
      afterIngestionStatForTest?.(path);
      const prefix = Buffer.allocUnsafe(Math.min(65_536, before.size));
      const chunk = Buffer.allocUnsafe(65_536);
      const hash = createHash("sha256");
      let position = 0;
      while (position < before.size) {
        const length = Math.min(chunk.length, before.size - position);
        const bytesRead = readSync(fd, chunk, 0, length, position);
        if (bytesRead === 0) throw new Error(`short ingestion read: ${path}`);
        const bytes = chunk.subarray(0, bytesRead);
        hash.update(bytes);
        if (position < prefix.length) {
          bytes.copy(prefix, position, 0, Math.min(bytes.length, prefix.length - position));
        }
        position += bytesRead;
      }
      return { prefix, sha256: hash.digest("hex") };
    },
  );
  const content = loaded.value;
  if (content === undefined) return undefined;
  return {
    abs: loaded.canonicalPath,
    mtimeMs: loaded.stat.mtimeMs,
    size: loaded.stat.size,
    sha256: content.sha256,
    text: content.prefix.toString("utf8"),
  };
}

function claimRelativePath(root: IngestionRoot, path: string): string {
  if (!path.startsWith(`${root.prefix}/`)) {
    throw new Error(`claimed path is outside ingestion root "${root.prefix}": ${path}`);
  }
  return path.slice(root.prefix.length + 1);
}

function hydrateClaim(root: IngestionRoot, claim: DoorClaim): DoorFile[] {
  if (!claim.files.every(validClaimFile)) {
    throw new Error("legacy claim content identity was not bound");
  }
  return claim.files.map((file) => {
    const loaded = loadIngestionFile(root, claimRelativePath(root, file.path));
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
): void {
  const claim = state.claim;
  if (!claim?.legacyCursor || claim.files.every(validClaimFile)) return;
  claim.files = claim.files.map((file) => {
    const loaded = loadIngestionFile(root, claimRelativePath(root, file.path));
    if (!loaded || loaded.mtimeMs !== file.mtimeMs) {
      throw new Error(`legacy claimed file changed before identity binding: ${file.path}`);
    }
    return {
      mtimeMs: file.mtimeMs,
      path: file.path,
      size: loaded.size,
      sha256: loaded.sha256,
    };
  });
  saveState(name, state);
}

function advanceFrontier(from: Frontier, files: FileOrder[]): Frontier {
  const mtimeMs = files[files.length - 1]!.mtimeMs;
  if (mtimeMs < from.mtimeMs) return from;
  const atFrontier = files
    .filter((file) => file.mtimeMs === mtimeMs)
    .map((file) => file.path);
  const seen = mtimeMs === from.mtimeMs ? [...from.seen, ...atFrontier] : atFrontier;
  return {
    mtimeMs,
    seen: [...new Set(seen)].sort(),
    ...(mtimeMs === from.mtimeMs && from.closedThroughPath !== undefined
      ? { closedThroughPath: from.closedThroughPath }
      : {}),
  };
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
    release = await acquireLock(name);
    if (!release) return { code: 75, detail: `door "${name}" is already running` };
    const state = loadState(name);
    const root = resolveIngestionRoot(spec.watch);

    if (state.paused) {
      return { code: 0, detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath(name)}` };
    }

    if (!state.claim) {
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
          const mtimeMs = loaded.mtimeMs;
          files.push({
            path: vaultPath,
            abs: loaded.abs,
            mtimeMs,
            size: loaded.size,
            sha256: loaded.sha256,
            text: loaded.text,
          });
        } catch (error: any) {
          if (error instanceof InitialFileMissingError) continue; // raced away before open
          throw error;
        }
      }
      files.sort(compareFile);
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
      saveState(name, state);
    }

    bindLegacyClaimIdentity(name, root, state);
    const claim = state.claim!;
    const matched = hydrateClaim(root, claim);
    const result = await agentRun(spec.prompt(matched), {
      label: `door-${name}`,
      ...spec.agent,
      idempotencyKey: claim.key,
    });
    if (!receiptDurable(result)) {
      return result.outcome === "retryable"
        ? { code: 75, detail: `agent retryable, ${matched.length} file(s) held: ${result.detail}` }
        : { code: 1, detail: `agent outcome indeterminate, ${matched.length} file(s) held: ${result.detail}` };
    }

    state.fires.push(Date.now());
    state.frontier = advanceFrontier(claim.from, claim.files);
    delete state.claim;
    saveState(name, state);
    return result.outcome === "succeeded"
      ? { code: 0, detail: `fired on ${matched.length} file(s): ${result.detail}` }
      : { code: 1, detail: `agent failed after claiming ${matched.length} file(s): ${result.detail}` };
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
