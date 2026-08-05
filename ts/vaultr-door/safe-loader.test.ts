import { afterEach, expect, test } from "bun:test";
import {
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  openRootBoundDirectory,
  StableFileIdentityError,
} from "./safe-loader.ts";

const roots: string[] = [];

function tempRoot(): string {
  const root = mkdtempSync(join(tmpdir(), "root-bound-test-"));
  roots.push(root);
  return root;
}

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

test("configured alias retarget fails before publication", () => {
  const base = tempRoot();
  const oldRoot = join(base, "old");
  const successor = join(base, "successor");
  const alias = join(base, "alias");
  mkdirSync(oldRoot);
  mkdirSync(successor);
  symlinkSync(oldRoot, alias);
  const root = openRootBoundDirectory(alias);
  const temp = root.createDurableFile("artifact.tmp", "held");

  unlinkSync(alias);
  symlinkSync(successor, alias);
  expect(() =>
    root.publishNoReplace("artifact.tmp", temp, "artifact.json")
  ).toThrow("configured directory changed");
  expect(existsSync(join(oldRoot, "artifact.json"))).toBe(false);
  expect(existsSync(join(successor, "artifact.json"))).toBe(false);

  temp.close();
  root.cleanup("artifact.tmp");
  root.close();
});

test("no-replace publication retains the final descriptor identity", () => {
  const directory = tempRoot();
  writeFileSync(join(directory, "artifact.json"), "existing");
  const root = openRootBoundDirectory(directory);
  expect(() => root.createDurableFile("artifact.json", "replacement"))
    .toThrow();
  expect(readFileSync(join(directory, "artifact.json"), "utf8")).toBe("existing");
  const conflict = root.createDurableFile("conflict.tmp", "replacement");
  expect(
    root.publishNoReplace("conflict.tmp", conflict, "artifact.json"),
  ).toBeUndefined();
  expect(conflict.verify().size).toBe(Buffer.byteLength("replacement"));
  expect(readFileSync(join(directory, "artifact.json"), "utf8")).toBe("existing");
  conflict.close();
  root.cleanup("conflict.tmp");

  const temp = root.createDurableFile("new.tmp", "published");
  const published = root.publishNoReplace("new.tmp", temp, "new.json");
  expect(published?.verify().size).toBe(Buffer.byteLength("published"));
  expect(existsSync(join(directory, "new.tmp"))).toBe(false);
  expect(readFileSync(join(directory, "new.json"), "utf8")).toBe("published");
  published?.close();
  temp.close();
  root.close();
});

test("temp substitution cannot publish through a retained descriptor", () => {
  const directory = tempRoot();
  const outside = join(directory, "outside");
  writeFileSync(outside, "hostile");
  const root = openRootBoundDirectory(directory);
  const temp = root.createDurableFile("artifact.tmp", "held");
  renameSync(join(directory, "artifact.tmp"), join(directory, "opened"));
  symlinkSync(outside, join(directory, "artifact.tmp"));

  expect(() =>
    root.publishNoReplace("artifact.tmp", temp, "artifact.json")
  ).toThrow(StableFileIdentityError);
  expect(existsSync(join(directory, "artifact.json"))).toBe(false);
  temp.close();
  root.close();
});

test("atomic replacement retains the published state descriptor", () => {
  const directory = tempRoot();
  writeFileSync(join(directory, "state.json"), "old");
  const root = openRootBoundDirectory(directory);
  const temp = root.createDurableFile("state.tmp", "new");
  const published = root.replaceAtomically("state.tmp", temp, "state.json");

  expect(readFileSync(join(directory, "state.json"), "utf8")).toBe("new");
  expect(existsSync(join(directory, "state.tmp"))).toBe(false);
  expect(published.verify().size).toBe(3);
  published.close();
  temp.close();
  root.close();
});

test("canonical path replacement cannot redirect held-directory operations", () => {
  const base = tempRoot();
  const directory = join(base, "state");
  const moved = join(base, "moved");
  mkdirSync(directory);
  const root = openRootBoundDirectory(directory);
  renameSync(directory, moved);
  mkdirSync(directory);

  expect(() => root.createDurableFile("state.tmp", "new"))
    .toThrow("retained canonical directory path changed");
  expect(existsSync(join(directory, "state.tmp"))).toBe(false);
  root.close();
});
