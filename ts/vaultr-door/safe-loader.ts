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

export interface StableRegularFileDescriptor {
  readonly fd: number;
  readonly stat: StableFileStat;
  readonly canonicalPath: string;
  verify(): StableFileStat;
  close(): void;
}

export class InitialFileMissingError extends Error {
  readonly code = "ENOENT";

  constructor(path: string, options: ErrorOptions) {
    super(`file was absent when opened: ${path}`, options);
  }
}

export class StableFileIdentityError extends Error {}

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

function inspectDescriptor(
  fd: number,
  canonicalRoot: string,
  relativePath: string,
  lexical: string,
): { stat: StableFileStat; canonicalPath: string } {
  const raw = fstatSync(fd);
  if (!raw.isFile()) {
    throw new Error(`not a regular file beneath canonical root: ${relativePath}`);
  }
  const stat = stableStat(raw);
  const canonicalPath = realpathSync(descriptorPath(fd));
  if (!isBeneath(canonicalRoot, canonicalPath)) {
    throw new Error(`opened file escapes canonical root: ${relativePath}`);
  }
  let lexicalStat: ReturnType<typeof lstatSync>;
  try {
    lexicalStat = lstatSync(lexical);
  } catch (error) {
    throw new StableFileIdentityError(
      `lexical file identity changed while open: ${relativePath}`,
      { cause: error },
    );
  }
  if (!lexicalStat.isFile()
    || lexicalStat.dev !== stat.dev
    || lexicalStat.ino !== stat.ino
    || lexicalStat.size !== stat.size
    || lexicalStat.mtimeMs !== stat.mtimeMs
    || lexicalStat.ctimeMs !== stat.ctimeMs) {
    throw new StableFileIdentityError(
      `lexical file identity changed while open: ${relativePath}`,
    );
  }
  const lexicalCanonical = realpathSync(lexical);
  if (!isBeneath(canonicalRoot, lexicalCanonical) || lexicalCanonical !== canonicalPath) {
    throw new StableFileIdentityError(
      `lexical file escapes canonical root: ${relativePath}`,
    );
  }
  return { stat, canonicalPath };
}

/** Retain one validated regular-file descriptor so callers can hold a kernel
 * lock and re-prove that the canonical pathname still names this same inode. */
export function openStableRegularFileDescriptor(
  canonicalRoot: string,
  relativePath: string,
  writable = false,
): StableRegularFileDescriptor {
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
    fd = openSync(
      lexical,
      (writable ? constants.O_RDWR : constants.O_RDONLY)
      | constants.O_NOFOLLOW
      | constants.O_NONBLOCK,
    );
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
    const opened = inspectDescriptor(fd, canonicalRoot, relativePath, lexical);
    let closed = false;
    return {
      fd,
      stat: opened.stat,
      canonicalPath: opened.canonicalPath,
      verify() {
        if (closed) throw new Error(`descriptor already closed: ${relativePath}`);
        const current = inspectDescriptor(fd, canonicalRoot, relativePath, lexical);
        if (!sameStat(opened.stat, current.stat)
          || current.canonicalPath !== opened.canonicalPath) {
          throw new StableFileIdentityError(
            `file changed while descriptor was retained: ${relativePath}`,
          );
        }
        return current.stat;
      },
      close() {
        if (!closed) {
          closed = true;
          closeSync(fd);
        }
      },
    };
  } catch (error) {
    closeSync(fd);
    throw error;
  }
}

export function readStableRegularDescriptor(
  handle: StableRegularFileDescriptor,
  relativePath: string,
  maxSize: number,
): Buffer {
  const before = handle.verify();
  if (before.size > maxSize) {
    throw new Error(`file exceeds ${maxSize} byte limit: ${relativePath}`);
  }
  const buffer = Buffer.allocUnsafe(before.size + 1);
  let offset = 0;
  while (offset < buffer.length) {
    const bytes = readSync(handle.fd, buffer, offset, buffer.length - offset, offset);
    if (bytes === 0) break;
    offset += bytes;
  }
  if (offset !== before.size) {
    throw new StableFileIdentityError(`file changed length while reading: ${relativePath}`);
  }
  handle.verify();
  return buffer.subarray(0, before.size);
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
  const handle = openStableRegularFileDescriptor(canonicalRoot, relativePath);
  try {
    const before = handle.stat;
    if (options.maxSize !== undefined && before.size > options.maxSize) {
      throw new Error(`file exceeds ${options.maxSize} byte limit: ${relativePath}`);
    }

    options.afterStat?.(relativePath);
    const value = reader(handle.fd, before);
    try {
      handle.verify();
    } catch (error) {
      if (error instanceof StableFileIdentityError) {
        throw new StableFileIdentityError(`file changed while reading: ${relativePath}`, {
          cause: error,
        });
      }
      throw error;
    }
    return { value, stat: before, canonicalPath: handle.canonicalPath };
  } finally {
    handle.close();
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
