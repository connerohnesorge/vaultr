import {
  closeSync,
  constants,
  fstatSync,
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
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";

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

function syncDirectory(path: string): void {
  const fd = openSync(path, "r");
  try {
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
}

export function ensureDirectoryDurable(path: string): void {
  const missing: string[] = [];
  let cursor = resolve(path);
  while (true) {
    try {
      if (!lstatSync(cursor).isDirectory()) {
        throw new Error(`${cursor} is not a real directory`);
      }
      break;
    } catch (error: any) {
      if (error?.code !== "ENOENT") throw error;
      missing.push(cursor);
      const parent = dirname(cursor);
      if (parent === cursor) throw error;
      cursor = parent;
    }
  }
  for (const directory of missing.reverse()) {
    try {
      mkdirSync(directory, { mode: 0o700 });
    } catch (error: any) {
      if (error?.code !== "EEXIST" || !lstatSync(directory).isDirectory()) throw error;
    }
    syncDirectory(directory);
    syncDirectory(dirname(directory));
  }
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

function sameInode(
  a: { dev: number; ino: number },
  b: { dev: number; ino: number },
): boolean {
  return a.dev === b.dev && a.ino === b.ino;
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

function validDirectoryEntry(name: string): boolean {
  return validRelativeFilePath(name) && !name.includes("/");
}

/**
 * Retains one canonical directory descriptor and routes entry publication and
 * directory fsync through that identity. Each publication revalidates the
 * configured path immediately before it becomes visible.
 */
export class RootBoundDirectory {
  readonly configuredPath: string;
  readonly canonicalPath: string;
  private readonly fd: number;
  private readonly identity: { dev: number; ino: number };
  private closed = false;

  constructor(path: string) {
    this.configuredPath = resolve(path);
    this.canonicalPath = canonicalDirectory(this.configuredPath);
    if (typeof constants.O_DIRECTORY !== "number"
      || typeof constants.O_NOFOLLOW !== "number") {
      throw new Error("retained no-follow directory opens are unavailable");
    }
    this.fd = openSync(
      this.canonicalPath,
      constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW,
    );
    const stat = fstatSync(this.fd);
    if (!stat.isDirectory()) {
      closeSync(this.fd);
      throw new Error(`${this.canonicalPath} is not a retained directory`);
    }
    this.identity = { dev: stat.dev, ino: stat.ino };
    try {
      this.verifyConfiguredPath();
    } catch (error) {
      closeSync(this.fd);
      throw error;
    }
  }

  displayPath(name: string): string {
    this.requireEntry(name);
    return resolve(this.configuredPath, name);
  }

  verify(): void {
    if (this.closed) throw new Error("root-bound directory is closed");
    const retained = fstatSync(this.fd);
    if (!retained.isDirectory() || !sameInode(this.identity, retained)) {
      throw new Error("retained canonical directory identity changed");
    }
    let lexical: ReturnType<typeof lstatSync>;
    try {
      lexical = lstatSync(this.canonicalPath);
    } catch (error) {
      throw new Error("retained canonical directory disappeared", {
        cause: error,
      });
    }
    if (!lexical.isDirectory() || !sameInode(this.identity, lexical)) {
      throw new Error("retained canonical directory path changed");
    }
  }

  verifyConfiguredPath(): void {
    this.verify();
    let canonical: string;
    try {
      canonical = realpathSync(this.configuredPath);
    } catch (error) {
      throw new Error("configured directory changed after acquisition", {
        cause: error,
      });
    }
    const stat = statSync(canonical);
    if (canonical !== this.canonicalPath
      || !stat.isDirectory()
      || !sameInode(this.identity, stat)) {
      throw new Error("configured directory changed after acquisition");
    }
  }

  readStableFile(
    name: string,
    maxSize: number,
    afterStat?: (path: string) => void,
  ): Buffer {
    this.requireEntry(name);
    this.verify();
    const value = readStableRegularFile(
      this.canonicalPath,
      name,
      maxSize,
      afterStat,
    ).value;
    this.verify();
    return value;
  }

  openStableFile(
    name: string,
    writable = false,
  ): StableRegularFileDescriptor {
    this.requireEntry(name);
    this.verify();
    const handle = openStableRegularFileDescriptor(
      this.canonicalPath,
      name,
      writable,
    );
    try {
      this.verify();
      return handle;
    } catch (error) {
      handle.close();
      throw error;
    }
  }

  /** Exclusively creates, writes, fsyncs, and retains one regular entry. */
  createDurableFile(
    name: string,
    bytes: string | Uint8Array,
  ): StableRegularFileDescriptor {
    const path = this.entryPath(name);
    this.verifyConfiguredPath();
    if (typeof constants.O_NOFOLLOW !== "number"
      || typeof constants.O_NONBLOCK !== "number") {
      throw new Error("nonblocking no-follow file creation is unavailable");
    }
    let fd: number | undefined;
    let retained: StableRegularFileDescriptor | undefined;
    let created = false;
    try {
      fd = openSync(
        path,
        constants.O_RDWR
        | constants.O_CREAT
        | constants.O_EXCL
        | constants.O_NOFOLLOW
        | constants.O_NONBLOCK,
        0o600,
      );
      created = true;
      const data = Buffer.from(bytes);
      let offset = 0;
      while (offset < data.length) {
        const written = writeSync(
          fd,
          data,
          offset,
          data.length - offset,
          offset,
        );
        if (written === 0) throw new Error(`short durable write: ${name}`);
        offset += written;
      }
      fsyncSync(fd);
      retained = this.openStableFile(name, true);
      if (!sameInode(fstatSync(fd), retained.stat)) {
        retained.close();
        throw new StableFileIdentityError(
          `created file identity changed: ${name}`,
        );
      }
      this.sync();
      closeSync(fd);
      fd = undefined;
      return retained;
    } catch (error) {
      if (fd !== undefined) closeSync(fd);
      retained?.close();
      if (created) {
        try {
          this.cleanup(name);
        } catch {}
      }
      throw error;
    }
  }

  /**
   * Hard-links a retained temp entry without replacement, fsyncs publication,
   * removes+fsyncs the temp name, and returns a descriptor bound to the final
   * entry. `undefined` means the final entry already existed.
   */
  publishNoReplace(
    tempName: string,
    temp: StableRegularFileDescriptor,
    finalName: string,
  ): StableRegularFileDescriptor | undefined {
    temp.verify();
    const tempPath = this.entryPath(tempName);
    const finalPath = this.entryPath(finalName);
    this.verifyConfiguredPath();
    try {
      linkSync(tempPath, finalPath);
    } catch (error: any) {
      if (error?.code === "EEXIST") return undefined;
      throw error;
    }
    let published: StableRegularFileDescriptor | undefined;
    try {
      this.sync();
      unlinkSync(tempPath);
      this.sync();
      published = this.openStableFile(finalName, true);
      if (!sameInode(fstatSync(temp.fd), published.stat)) {
        throw new StableFileIdentityError(
          `published file identity changed: ${finalName}`,
        );
      }
      return published;
    } catch (error) {
      published?.close();
      try {
        unlinkSync(finalPath);
        this.sync();
      } catch {}
      throw error;
    }
  }

  /** Atomically replaces one entry from a retained temp and fsyncs the directory. */
  replaceAtomically(
    tempName: string,
    temp: StableRegularFileDescriptor,
    finalName: string,
  ): StableRegularFileDescriptor {
    temp.verify();
    const tempPath = this.entryPath(tempName);
    const finalPath = this.entryPath(finalName);
    this.verifyConfiguredPath();
    renameSync(tempPath, finalPath);
    const published = this.openStableFile(finalName, true);
    try {
      if (!sameInode(fstatSync(temp.fd), published.stat)) {
        throw new StableFileIdentityError(
          `replacement file identity changed: ${finalName}`,
        );
      }
      this.sync();
      return published;
    } catch (error) {
      published.close();
      throw error;
    }
  }

  removeAndSync(name: string): void {
    unlinkSync(this.entryPath(name));
    this.sync();
  }

  cleanup(name: string): void {
    try {
      this.removeAndSync(name);
    } catch (error: any) {
      if (error?.code !== "ENOENT") throw error;
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    closeSync(this.fd);
  }

  private requireEntry(name: string): void {
    if (!validDirectoryEntry(name)) {
      throw new Error(`invalid root-bound directory entry: ${name}`);
    }
  }

  private entryPath(name: string): string {
    this.requireEntry(name);
    this.verify();
    return resolve(this.canonicalPath, name);
  }

  private sync(): void {
    this.verify();
    fsyncSync(this.fd);
  }
}

export function openRootBoundDirectory(path: string): RootBoundDirectory {
  return new RootBoundDirectory(path);
}
