import { createHash } from "node:crypto";
import { readSync, realpathSync, statSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";
import {
  validRelativeFilePath,
  withStableRegularFile,
} from "./safe-loader.ts";
import { testHooks } from "./test-hooks.ts";

const HOME = process.env.HOME ?? "";
const vaultRoot = () =>
  process.env.DOOR_VAULT_ROOT ?? `${HOME}/.dotfiles/vault`;
const allowedRoots = () =>
  (process.env.DOOR_ROOTS ?? "mail,teams,tickets")
    .split(",")
    .map((root) => root.trim())
    .filter(Boolean);

export interface IngestionRoot {
  prefix: string;
  path: string;
  pattern: string;
}

export interface LoadedIngestionFile {
  abs: string;
  mtimeMs: number;
  size: number;
  sha256: string;
  text: string;
}

function isBeneath(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === ""
    || (!isAbsolute(rel) && rel !== ".." && !rel.startsWith(`..${sep}`));
}

function validRelativePath(path: string, allowGlob: boolean): boolean {
  if (!path || isAbsolute(path) || path.includes("\\") || path.includes("\0")) return false;
  return path.split("/").every((segment) =>
    segment.length > 0
    && segment !== "."
    && segment !== ".."
    && (allowGlob || !/[*?[\]{}]/.test(segment)));
}

export function isExplicitHiddenLiteral(segment: string): boolean {
  return segment.startsWith(".") && !/[*?[\]{}]/.test(segment);
}

export function hiddenSegmentsAllowed(pattern: string, path: string): boolean {
  const expected = pattern.split("/").filter(isExplicitHiddenLiteral);
  const actual = path.split("/").filter((segment) => segment.startsWith("."));
  return expected.length === actual.length
    && expected.every((segment, index) => segment === actual[index]);
}

/** Resolve one trusted ingestion root and the watch pattern beneath it. */
export function resolveIngestionRoot(watch: string): IngestionRoot {
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
  const prefix = roots.find((root) =>
    watch === root || watch.startsWith(`${root}/`));
  if (!prefix) {
    throw new Error(
      `watch "${watch}" rejected: must be beneath an ingestion root (${roots.join(", ")})`,
    );
  }
  const vault = realpathSync(vaultRoot());
  const path = realpathSync(resolve(vault, prefix));
  if (!isBeneath(vault, path) || !statSync(path).isDirectory()) {
    throw new Error(`ingestion root "${prefix}" escapes the vault or is not a directory`);
  }
  return {
    prefix,
    path,
    pattern: watch === prefix ? "*" : watch.slice(prefix.length + 1),
  };
}

export function loadIngestionFile(
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
      testHooks.afterIngestionStat?.(path);
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
          bytes.copy(
            prefix,
            position,
            0,
            Math.min(bytes.length, prefix.length - position),
          );
        }
        position += bytesRead;
      }
      return { prefix, sha256: hash.digest("hex") };
    },
  );
  if (loaded.value === undefined) return undefined;
  return {
    abs: loaded.canonicalPath,
    mtimeMs: loaded.stat.mtimeMs,
    size: loaded.stat.size,
    sha256: loaded.value.sha256,
    text: loaded.value.prefix.toString("utf8"),
  };
}

export function claimRelativePath(root: IngestionRoot, path: string): string {
  if (!path.startsWith(`${root.prefix}/`)) {
    throw new Error(
      `claimed path is outside ingestion root "${root.prefix}": ${path}`,
    );
  }
  return path.slice(root.prefix.length + 1);
}
