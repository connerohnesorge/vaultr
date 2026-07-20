import { createHash, randomUUID } from "node:crypto";
import { dlopen, FFIType } from "bun:ffi";
import { isAbsolute } from "node:path";
import {
  ensureDirectoryDurable,
  InitialFileMissingError,
  openRootBoundDirectory,
  readStableRegularDescriptor,
  type RootBoundDirectory,
  StableFileIdentityError,
  type StableRegularFileDescriptor,
} from "./safe-loader.ts";
import { testHooks } from "./test-hooks.ts";

const HOME = process.env.HOME ?? "";
const configuredStateDir = () =>
  process.env.DOOR_STATE_DIR ?? `${HOME}/.local/state/plant/doors`;
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

export interface FileOrder {
  mtimeMs: number;
  path: string;
}

export interface ClaimFile extends FileOrder {
  size: number;
  sha256: string;
}

type LegacyClaimFile = FileOrder;

export interface Frontier {
  mtimeMs: number;
  seen: string[];
  closedThroughPath?: string;
}

interface LegacyCursor {
  mtimeMs: number;
  path: string;
}

export interface DoorClaim {
  from: Frontier;
  files: Array<ClaimFile | LegacyClaimFile>;
  key: string;
  legacyCursor?: LegacyCursor;
}

export interface DoorState {
  version: 2;
  frontier: Frontier;
  fires: number[];
  paused?: string;
  claim?: DoorClaim;
}

export type DoorStateStore = RootBoundDirectory;

function openStateStore(): DoorStateStore {
  const path = configuredStateDir();
  ensureDirectoryDurable(path);
  return openRootBoundDirectory(path);
}

function readControlFile(
  store: DoorStateStore,
  name: string,
): Buffer | undefined {
  try {
    return store.readStableFile(
      name,
      MAX_STATE_BYTES,
      testHooks.afterMetadataStat,
    );
  } catch (error) {
    if (error instanceof InitialFileMissingError) return undefined;
    throw error;
  }
}

export function compareFile(a: FileOrder, b: FileOrder): number {
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

function validRelativePath(path: string): boolean {
  if (!path || isAbsolute(path) || path.includes("\\") || path.includes("\0")) return false;
  return path.split("/").every((segment) =>
    segment.length > 0
    && segment !== "."
    && segment !== "..");
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

export function validClaimFile(value: unknown): value is ClaimFile {
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
      : validRelativePath(cursor.path));
}

function validFrontier(value: unknown): value is Frontier {
  const frontier = value as Frontier;
  return !!frontier
    && validTimestamp(frontier.mtimeMs)
    && Array.isArray(frontier.seen)
    && frontier.seen.every((path) => typeof path === "string" && path.length > 0)
    && frontier.seen.every(validRelativePath)
    && frontier.seen.every((path, index) => index === 0 || frontier.seen[index - 1]! < path)
    && (frontier.closedThroughPath === undefined
      || (typeof frontier.closedThroughPath === "string"
        && (frontier.closedThroughPath === ""
          || validRelativePath(frontier.closedThroughPath))
        && frontier.seen.every((path) => path > frontier.closedThroughPath!)));
}

export function isNew(file: FileOrder, frontier: Frontier): boolean {
  return file.mtimeMs > frontier.mtimeMs
    || (file.mtimeMs === frontier.mtimeMs
      && (frontier.closedThroughPath === undefined || file.path > frontier.closedThroughPath)
      && !frontier.seen.includes(file.path));
}

export function claimKey(
  name: string,
  from: Frontier,
  files: ClaimFile[],
): string {
  return createHash("sha256")
    .update(JSON.stringify({ door: name, from, files }))
    .digest("hex");
}

function legacyClaimKey(
  name: string,
  from: LegacyCursor,
  files: FileOrder[],
): string {
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
  if (raw.claim === undefined) return state;

  const claim = raw.claim as DoorClaim;
  const legacy = claim?.legacyCursor;
  const files = Array.isArray(claim?.files) ? claim.files : [];
  const identifiedFiles = files.length > 0 && files.every(validClaimFile);
  const unboundLegacyFiles = files.length > 0 && files.every(validLegacyClaimFile);
  if (!claim || !validFrontier(claim.from) || !sameFrontier(claim.from, state.frontier)
    || files.length === 0
    || (legacy ? !identifiedFiles && !unboundLegacyFiles : !identifiedFiles)
    || claim.files.some((file) => !validRelativePath(file.path))
    || new Set(claim.files.map((file) => file.path)).size !== claim.files.length
    || claim.files.some((file, index) =>
      (legacy ? compareFile(file, legacy) <= 0 : !isNew(file, claim.from))
      || (index > 0 && compareFile(claim.files[index - 1]!, file) >= 0))
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
  if (raw.claim === undefined) return state;

  const claim = raw.claim as {
    from: LegacyCursor;
    files: LegacyClaimFile[];
    key: string;
  };
  if (!claim || !validLegacyCursor(claim.from)
    || compareFile(claim.from, cursor) !== 0
    || !Array.isArray(claim.files) || claim.files.length === 0
    || !claim.files.every(validLegacyClaimFile)
    || claim.files.some((file) => !validRelativePath(file.path))
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
  return state;
}

export function loadState(store: DoorStateStore, name: string): DoorState {
  const bytes = readControlFile(store, `${name}.json`);
  if (bytes === undefined) {
    return { version: 2, frontier: { mtimeMs: 0, seen: [] }, fires: [] };
  }
  try {
    const value = JSON.parse(bytes.toString("utf8"));
    if ((value as Record<string, unknown>)?.version === 2) {
      return parseState(name, value);
    }
    const state = parseState(name, migrateLegacyState(name, value));
    saveState(store, name, state);
    return state;
  } catch (error) {
    throw new Error(
      `cannot read supported ${store.displayPath(`${name}.json`)}: `
      + `${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

export function saveState(
  store: DoorStateStore,
  name: string,
  state: DoorState,
): void {
  const final = `${name}.json`;
  const temp = `${final}.tmp-${randomUUID()}`;
  let tempHandle: StableRegularFileDescriptor | undefined;
  try {
    tempHandle = store.createDurableFile(temp, `${JSON.stringify(state)}\n`);
    testHooks.beforeStatePublish?.();
    store.replaceAtomically(temp, tempHandle, final).close();
  } catch (error) {
    try {
      store.cleanup(temp);
    } catch {}
    throw error;
  } finally {
    tempHandle?.close();
  }
}

class IncompleteLockOwner extends Error {}

function parseLockOwner(
  bytes: Buffer,
  path: string,
): { pid: number; token: string } {
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

function lockOwner(
  store: DoorStateStore,
  name: string,
): { pid: number; token: string } | undefined {
  const path = store.displayPath(name);
  const bytes = readControlFile(store, name);
  return bytes === undefined ? undefined : parseLockOwner(bytes, path);
}

async function readPublishedLockOwner(
  store: DoorStateStore,
  name: string,
): Promise<{ pid: number; token: string } | undefined> {
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      return lockOwner(store, name);
    } catch (error) {
      if (!(error instanceof IncompleteLockOwner)) throw error;
      if (attempt < 3) await Bun.sleep(10);
    }
  }
  throw new Error(
    `incomplete legacy door lock ${store.displayPath(name)}; refusing automatic removal `
    + "until an offline migration verifies no legacy Door process exists",
  );
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error: any) {
    return error?.code === "EPERM";
  }
}

async function recoverStaleLock(
  store: DoorStateStore,
  name: string,
  observed: { pid: number; token: string },
): Promise<boolean> {
  const path = store.displayPath(name);
  if (!kernelLock) {
    throw new Error(`kernel-backed recovery unavailable; refusing to remove stale lock ${path}`);
  }
  let handle: StableRegularFileDescriptor;
  try {
    handle = store.openStableFile(name, true);
  } catch (error) {
    if (error instanceof InitialFileMissingError) return false;
    throw error;
  }
  let locked = false;
  try {
    await testHooks.beforeStaleRecovery?.(observed);
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
        readStableRegularDescriptor(handle, name, MAX_STATE_BYTES),
        path,
      );
    } catch (error) {
      if (error instanceof StableFileIdentityError) return false;
      throw error;
    }
    if (current.pid !== observed.pid
      || current.token !== observed.token
      || pidAlive(current.pid)) {
      return false;
    }
    await testHooks.afterStaleOwnerRead?.(current);
    try {
      const rechecked = parseLockOwner(
        readStableRegularDescriptor(handle, name, MAX_STATE_BYTES),
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
    store.removeAndSync(name);
    return true;
  } finally {
    if (locked) kernelLock.symbols.flock(handle.fd, LOCK_UN);
    handle.close();
  }
}

function publishLock(
  store: DoorStateStore,
  name: string,
  owner: { pid: number; token: string },
): boolean {
  const temp = `${name}.tmp-${owner.token}`;
  let tempHandle: StableRegularFileDescriptor | undefined;
  let linked = false;
  try {
    tempHandle = store.createDurableFile(
      temp,
      `${JSON.stringify(owner)}\n`,
    );
    const published = store.publishNoReplace(temp, tempHandle, name);
    linked = published !== undefined;
    published?.close();
    return linked;
  } catch (error) {
    if (linked) {
      try {
        store.removeAndSync(name);
      } catch {}
    }
    throw error;
  } finally {
    tempHandle?.close();
    try {
      store.cleanup(temp);
    } catch {}
  }
}

export interface DoorLease {
  store: DoorStateStore;
  release(): void;
}

export async function acquireLock(name: string): Promise<DoorLease | undefined> {
  const store = openStateStore();
  const lockName = `${name}.lock`;
  let acquired = false;
  try {
    for (let attempt = 0; attempt < 3; attempt++) {
      store.verifyConfiguredPath();
      const token = randomUUID();
      const published = publishLock(store, lockName, { pid: process.pid, token });
      const owner = published
        ? undefined
        : await readPublishedLockOwner(store, lockName);
      if (owner && pidAlive(owner.pid)) return undefined;
      if (owner) {
        await recoverStaleLock(store, lockName, owner);
      }
      if (!published) continue;
      acquired = true;
      let released = false;
      return {
        store,
        release() {
          if (released) return;
          released = true;
          try {
            if (lockOwner(store, lockName)?.token !== token) {
              throw new Error(`door lock ownership changed for ${name}`);
            }
            store.removeAndSync(lockName);
          } finally {
            store.close();
          }
        },
      };
    }
    throw new Error(`could not recover stale door lock ${store.displayPath(lockName)}`);
  } finally {
    if (!acquired) store.close();
  }
}

export function advanceFrontier(
  from: Frontier,
  files: FileOrder[],
): Frontier {
  const mtimeMs = files[files.length - 1]!.mtimeMs;
  if (mtimeMs < from.mtimeMs) return from;
  const atFrontier = files
    .filter((file) => file.mtimeMs === mtimeMs)
    .map((file) => file.path);
  const seen = mtimeMs === from.mtimeMs
    ? [...from.seen, ...atFrontier]
    : atFrontier;
  return {
    mtimeMs,
    seen: [...new Set(seen)].sort(),
    ...(mtimeMs === from.mtimeMs && from.closedThroughPath !== undefined
      ? { closedThroughPath: from.closedThroughPath }
      : {}),
  };
}
