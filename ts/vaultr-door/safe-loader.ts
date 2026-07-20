import {
  closeSync,
  constants,
  fstatSync,
  lstatSync,
  openSync,
  readSync,
  realpathSync,
  statSync,
} from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";

export interface StableFileStat {
  dev: number;
  ino: number;
  size: number;
  mtimeMs: number;
  ctimeMs: number;
}

export interface StableFile<T> {
  value: T;
  stat: StableFileStat;
  canonicalPath: string;
}

export interface StableFileOptions {
  /** Reject a stable regular file larger than this before invoking the reader. */
  maxSize?: number;
  /** Test-only barrier after descriptor validation and before the reader. */
  afterStat?: (path: string) => void;
}

export class InitialFileMissingError extends Error {
  readonly code = "ENOENT";

  constructor(path: string, options: ErrorOptions) {
    super(`file was absent when opened: ${path}`, options);
  }
}

function isBeneath(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (!isAbsolute(rel) && rel !== ".." && !rel.startsWith(`..${sep}`));
}

export function validRelativeFilePath(path: string): boolean {
  if (!path || isAbsolute(path) || path.includes("\\") || path.includes("\0")) return false;
  return path.split("/").every((segment) =>
    segment.length > 0
    && segment !== "."
    && segment !== ".."
    && !/[*?[\]{}]/.test(segment));
}

export function canonicalDirectory(path: string): string {
  const canonical = realpathSync(path);
  if (!statSync(canonical).isDirectory()) {
    throw new Error(`${path} is not a directory`);
  }
  return canonical;
}

function descriptorPath(fd: number): string {
  if (process.platform === "darwin") return `/dev/fd/${fd}`;
  if (process.platform === "linux") return `/proc/self/fd/${fd}`;
  throw new Error("descriptor identity validation is unavailable");
}

function stableStat(stat: ReturnType<typeof fstatSync>): StableFileStat {
  if (!Number.isSafeInteger(stat.size) || stat.size < 0) {
    throw new Error("file size cannot be represented safely");
  }
  return {
    dev: stat.dev,
    ino: stat.ino,
    size: stat.size,
    mtimeMs: stat.mtimeMs,
    ctimeMs: stat.ctimeMs,
  };
}

function sameStat(a: StableFileStat, b: StableFileStat): boolean {
  return a.dev === b.dev
    && a.ino === b.ino
    && a.size === b.size
    && a.mtimeMs === b.mtimeMs
    && a.ctimeMs === b.ctimeMs;
}

/**
 * Open one traversal-free path beneath a canonical root with no-follow and
 * nonblocking semantics, read through that exact descriptor, then prove both
 * the descriptor and current lexical pathname still identify the same stable
 * regular file beneath the root.
 */
export function withStableRegularFile<T>(
  canonicalRoot: string,
  relativePath: string,
  options: StableFileOptions,
  reader: (fd: number, stat: StableFileStat) => T,
): StableFile<T> {
  if (!validRelativeFilePath(relativePath)) {
    throw new Error(`invalid relative file path: ${relativePath}`);
  }
  if (typeof constants.O_NOFOLLOW !== "number" || typeof constants.O_NONBLOCK !== "number") {
    throw new Error("nonblocking no-follow file opens are unavailable");
  }
  const lexical = resolve(canonicalRoot, relativePath);
  if (!isBeneath(canonicalRoot, lexical)) {
    throw new Error(`path escapes canonical root: ${relativePath}`);
  }

  let fd: number;
  try {
    fd = openSync(lexical, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  } catch (error: any) {
    if (error?.code === "ENOENT") {
      throw new InitialFileMissingError(relativePath, { cause: error });
    }
    if (error?.code === "ELOOP") {
      throw new Error(`symlink rejected beneath canonical root: ${relativePath}`);
    }
    throw error;
  }
  try {
    const beforeRaw = fstatSync(fd);
    if (!beforeRaw.isFile()) {
      throw new Error(`not a regular file beneath canonical root: ${relativePath}`);
    }
    const before = stableStat(beforeRaw);
    if (options.maxSize !== undefined && before.size > options.maxSize) {
      throw new Error(`file exceeds ${options.maxSize} byte limit: ${relativePath}`);
    }
    const openedBefore = realpathSync(descriptorPath(fd));
    if (!isBeneath(canonicalRoot, openedBefore)) {
      throw new Error(`opened file escapes canonical root: ${relativePath}`);
    }

    options.afterStat?.(relativePath);
    const value = reader(fd, before);

    const afterRaw = fstatSync(fd);
    if (!afterRaw.isFile()) {
      throw new Error(`opened file changed type: ${relativePath}`);
    }
    const after = stableStat(afterRaw);
    if (!sameStat(before, after)) {
      throw new Error(`file changed while reading: ${relativePath}`);
    }
    const openedAfter = realpathSync(descriptorPath(fd));
    if (!isBeneath(canonicalRoot, openedAfter) || openedAfter !== openedBefore) {
      throw new Error(`opened file moved outside canonical root: ${relativePath}`);
    }

    const lexicalStat = lstatSync(lexical);
    if (!lexicalStat.isFile()
      || lexicalStat.dev !== before.dev
      || lexicalStat.ino !== before.ino
      || lexicalStat.size !== before.size
      || lexicalStat.mtimeMs !== before.mtimeMs
      || lexicalStat.ctimeMs !== before.ctimeMs) {
      throw new Error(`lexical file identity changed while reading: ${relativePath}`);
    }
    const lexicalCanonical = realpathSync(lexical);
    if (!isBeneath(canonicalRoot, lexicalCanonical) || lexicalCanonical !== openedAfter) {
      throw new Error(`lexical file escapes canonical root: ${relativePath}`);
    }
    return { value, stat: before, canonicalPath: openedAfter };
  } finally {
    closeSync(fd);
  }
}

export function readStableRegularFile(
  canonicalRoot: string,
  relativePath: string,
  maxSize: number,
  afterStat?: (path: string) => void,
): StableFile<Buffer> {
  return withStableRegularFile(
    canonicalRoot,
    relativePath,
    { maxSize, afterStat },
    (fd, stat) => {
      const buffer = Buffer.allocUnsafe(stat.size + 1);
      let offset = 0;
      while (offset < buffer.length) {
        const bytes = readSync(fd, buffer, offset, buffer.length - offset, offset);
        if (bytes === 0) break;
        offset += bytes;
      }
      if (offset !== stat.size) {
        throw new Error(`file changed length while reading: ${relativePath}`);
      }
      return buffer.subarray(0, stat.size);
    },
  );
}
