import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  mkdirSync,
  readFileSync,
  renameSync,
  symlinkSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { door } from "./index.ts";
import {
  landFile,
  setupDoorTest,
  spawnWorker,
  stubCalls,
  stubLog,
  teardownDoorTest,
  tmp,
  vault,
} from "./test-support.ts";
import { testHooks } from "./test-hooks.ts";

beforeEach(setupDoorTest);
afterEach(teardownDoorTest);

test("non-ingestion watch roots are rejected before any launch", async () => {
  mkdirSync(join(vault, "learnings"), { recursive: true });
  landFile("learnings/x.md", 1000);
  const res = await door({ name: "t", watch: "learnings/**/*.md", prompt: () => "go" });
  expect(res.code).toBe(1);
  expect(res.detail).toContain("rejected");
  expect(stubCalls()).toBe(0);
});

test("watch traversal is rejected before any scan or launch", async () => {
  landFile("mail/a.md", 1000);
  const result = await door({ name: "t", watch: "mail/../mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("traversal-free");
  expect(stubCalls()).toBe(0);
});

test("a scanned symlink escaping the ingestion root fails closed", async () => {
  const outside = join(tmp, "outside.md");
  writeFileSync(outside, "hostile");
  symlinkSync(outside, join(vault, "mail", "escape.md"));
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("symlink rejected beneath canonical root");
  expect(stubCalls()).toBe(0);
});

test("an unrelated escaping symlink outside the glob does not disable the door", async () => {
  const outside = join(tmp, "outside.txt");
  writeFileSync(outside, "unrelated");
  symlinkSync(outside, join(vault, "mail", "escape.txt"));
  landFile("mail/a.md", 1000);
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(0);
  expect(stubCalls()).toBe(1);
});

test("a matching fifo is rejected without blocking", async () => {
  const fifo = join(vault, "mail", "pipe.md");
  expect(Bun.spawnSync(["mkfifo", fifo]).exitCode).toBe(0);
  const worker = spawnWorker();
  const code = await Promise.race([
    worker.exited,
    Bun.sleep(1000).then(() => -1),
  ]);
  if (code === -1) worker.kill();
  expect(code).toBe(0);
  expect(stubCalls()).toBe(0);
});

test("a pathname swapped after the first stat fails before launch", async () => {
  landFile("mail/a.md", 1000);
  const outside = join(tmp, "outside.md");
  writeFileSync(outside, "hostile outside content");
  let opens = 0;
  testHooks.afterIngestionStat = (path) => {
    if (path !== "a.md" || ++opens !== 2) return;
    renameSync(join(vault, "mail", "a.md"), join(vault, "mail", "a.opened"));
    symlinkSync(outside, join(vault, "mail", "a.md"));
  };

  const result = await door({
    name: "t",
    watch: "mail/*.md",
    prompt: (files) => files.map((file) => file.text).join("\n"),
  });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("file changed while reading");
  expect(opens).toBe(2);
  expect(stubCalls()).toBe(0);
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeDefined();
});

test("an in-place write after the first stat fails and retries the stable file once", async () => {
  landFile("mail/a.md", 1000);
  await Bun.sleep(10);
  let reads = 0;
  testHooks.afterIngestionStat = (path) => {
    if (path !== "a.md" || ++reads !== 1) return;
    writeFileSync(join(vault, "mail", path), "changed of mail/a.md");
    utimesSync(join(vault, "mail", path), 1000, 1000);
  };
  const spec = {
    name: "t",
    watch: "mail/*.md",
    prompt: (files: any[]) => files[0].text,
  };

  const changed = await door(spec);
  expect(changed.code).toBe(1);
  expect(changed.detail).toContain("file changed while reading");
  expect(stubCalls()).toBe(0);

  delete testHooks.afterIngestionStat;
  expect((await door(spec)).code).toBe(0);
  expect(readFileSync(stubLog, "utf8")).toContain("changed of mail/a.md");
  expect((await door(spec)).detail).toBe("no new files");
  expect(stubCalls()).toBe(1);
});

test("large files contribute at most 65,536 bytes to the prompt", async () => {
  const path = join(vault, "mail", "large.md");
  writeFileSync(path, `${"a".repeat(65_536)}outside-read-limit`);
  utimesSync(path, 1000, 1000);

  const result = await door({
    name: "t",
    watch: "mail/*.md",
    prompt: (files) => `${Buffer.byteLength(files[0]!.text)}:${files[0]!.text.includes("outside-read-limit")}`,
  });
  expect(result.code).toBe(0);
  expect(readFileSync(stubLog, "utf8")).toContain("65536:false");
  expect(stubCalls()).toBe(1);
});

test("an explicitly watched hidden ingestion directory is scanned", async () => {
  landFile("mail/.door/summons/a.json", 1000);
  landFile("mail/.door/summons/.hidden.json", 1001);

  const result = await door({
    name: "t",
    watch: "mail/.door/summons/*.json",
    prompt: (files) => files.map((file) => file.path).join("\n"),
  });
  expect(result.code).toBe(0);
  expect(stubCalls()).toBe(1);
  expect(readFileSync(stubLog, "utf8")).toContain("mail/.door/summons/a.json");
  expect(readFileSync(stubLog, "utf8")).not.toContain(".hidden.json");
});

test("globstar cannot consume an unexpressed hidden directory or leaf", async () => {
  landFile("mail/.door/visible/a.json", 1000);
  landFile("mail/.door/.private/b.json", 1001);
  landFile("mail/.door/visible/.leaf.json", 1002);

  const result = await door({
    name: "t",
    watch: "mail/.door/**/*.json",
    prompt: (files) => files.map((file) => file.path).join("\n"),
  });
  expect(result.code).toBe(0);
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("mail/.door/visible/a.json");
  expect(log).not.toContain("mail/.door/.private/b.json");
  expect(log).not.toContain("mail/.door/visible/.leaf.json");
});

test("an ordinary watch does not broaden to hidden files", async () => {
  landFile("mail/visible.md", 1000);
  landFile("mail/.secret.md", 1001);

  const result = await door({
    name: "t",
    watch: "mail/*.md",
    prompt: (files) => files.map((file) => file.path).join("\n"),
  });
  expect(result.code).toBe(0);
  expect(stubCalls()).toBe(1);
  expect(readFileSync(stubLog, "utf8")).toContain("mail/visible.md");
  expect(readFileSync(stubLog, "utf8")).not.toContain("mail/.secret.md");
});

test("files behind the frontier are not read again", async () => {
  landFile("mail/a.md", 1000);
  let reads = 0;
  testHooks.afterIngestionStat = () => reads++;
  const spec = { name: "t", watch: "mail/*.md", prompt: () => "go" };

  expect((await door(spec)).code).toBe(0);
  expect(reads).toBe(2);
  expect((await door(spec)).detail).toBe("no new files");
  expect(reads).toBe(2);
});

test("a claimed file replaced by an escaping symlink is rejected before retry launch", async () => {
  landFile("mail/a.md", 1000);
  const crashed = spawnWorker({ CRASH_IN_PROMPT: "1" });
  expect(await crashed.exited).toBe(86);
  const outside = join(tmp, "outside.md");
  writeFileSync(outside, "hostile");
  unlinkSync(join(vault, "mail", "a.md"));
  symlinkSync(outside, join(vault, "mail", "a.md"));

  const retried = spawnWorker();
  expect(await retried.exited).toBe(1);
  expect(stubCalls()).toBe(0);
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeDefined();
});

test("filter narrows the batch, including by content", async () => {
  landFile("mail/keep.md", 1000);
  landFile("mail/skip.md", 1001);
  await door({ name: "t", watch: "mail/*.md", filter: (f) => f.text.includes("of mail/keep"), prompt: (fs) => fs.map((f) => f.path).join(" ") });
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("keep.md");
  expect(log).not.toContain("skip.md");
});
