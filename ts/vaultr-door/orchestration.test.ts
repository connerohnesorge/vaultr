import { afterEach, beforeEach, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  chmodSync,
  readFileSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { door } from "./index.ts";
import {
  landFile,
  setupDoorTest,
  spawnWorker,
  stub,
  stubCalls,
  stubLog,
  teardownDoorTest,
  tmp,
  vault,
  writeIdempotentStub,
  writeStub,
} from "./test-support.ts";

beforeEach(setupDoorTest);
afterEach(teardownDoorTest);

test("batch fires once and the fence prevents a double fire", async () => {
  landFile("mail/a.md", 1000);
  landFile("mail/b.md", 1001);
  const spec = { name: "t", watch: "mail/**/*.md", prompt: (fs: any[]) => `triage ${fs.map((f) => f.path).join(" ")}` };
  const first = await door(spec);
  expect(first.code).toBe(0);
  expect(stubCalls()).toBe(1);
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("mail/a.md");
  expect(log).toContain("mail/b.md");
  expect(log).toContain("--label door-t");
  expect(log).toContain("--idempotency-key");

  const second = await door(spec);
  expect(second.code).toBe(0);
  expect(second.detail).toBe("no new files");
  expect(stubCalls()).toBe(1);
});

test("a retryable result does not advance the frontier; files fire on the next run", async () => {
  landFile("mail/a.md", 1000);
  const spec = { name: "t", watch: "mail/*.md", prompt: () => "go" };
  writeStub(75);
  const held = await door(spec);
  expect(held.code).toBe(75);
  writeStub(0);
  const retried = await door(spec);
  expect(retried.code).toBe(0);
  expect(retried.detail).toContain("fired on 1 file");
});

test("a durable failed outcome advances the frontier without a second launch", async () => {
  landFile("mail/a.md", 1000);
  const spec = { name: "t", watch: "mail/*.md", prompt: () => "go" };
  writeStub(1);
  expect((await door(spec)).code).toBe(1);
  writeStub(0);
  expect((await door(spec)).detail).toBe("no new files");
  expect(stubCalls()).toBe(1);
});

test("a pre-launch crash resumes the persisted ordered claim and key", async () => {
  landFile("mail/a.md", 1000);
  const crashed = spawnWorker({ CRASH_IN_PROMPT: "1" });
  expect(await crashed.exited).toBe(86);
  expect(stubCalls()).toBe(0);
  const claimed = JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8"));
  expect(claimed.claim.files[0]).toMatchObject({
    mtimeMs: claimed.claim.files[0].mtimeMs,
    path: "mail/a.md",
    size: Buffer.byteLength("content of mail/a.md"),
  });
  expect(claimed.claim.files[0].sha256).toMatch(/^[0-9a-f]{64}$/);
  expect(claimed.claim.key).toBe(
    createHash("sha256").update(JSON.stringify({
      door: "t",
      from: claimed.claim.from,
      files: claimed.claim.files,
    })).digest("hex"),
  );

  const retried = spawnWorker();
  expect(await retried.exited).toBe(0);
  expect(stubCalls()).toBe(1);
});

test("a same-path same-mtime regular replacement cannot hydrate a persisted claim", async () => {
  landFile("mail/a.md", 1000);
  const crashed = spawnWorker({ CRASH_IN_PROMPT: "1" });
  expect(await crashed.exited).toBe(86);
  const statePath = join(tmp, "state", "t.json");
  const before = JSON.parse(readFileSync(statePath, "utf8"));
  expect(before.claim.files[0].size).toBe(Buffer.byteLength("content of mail/a.md"));

  writeFileSync(join(vault, "mail", "a.md"), "changed of mail/a.md");
  utimesSync(join(vault, "mail", "a.md"), 1000, 1000);
  const retried = spawnWorker();
  expect(await retried.exited).toBe(1);
  expect(stubCalls()).toBe(0);
  const after = JSON.parse(readFileSync(statePath, "utf8"));
  expect(after.claim.key).toBe(before.claim.key);
  expect(after.claim.files).toEqual(before.claim.files);
});

test("a same-size same-mtime replacement after 64 KiB cannot hydrate a persisted claim", async () => {
  const path = join(vault, "mail", "a.md");
  const prefix = "a".repeat(65_536);
  writeFileSync(path, `${prefix}original-tail`);
  utimesSync(path, 1000, 1000);
  const crashed = spawnWorker({ CRASH_IN_PROMPT: "1" });
  expect(await crashed.exited).toBe(86);
  const statePath = join(tmp, "state", "t.json");
  const before = JSON.parse(readFileSync(statePath, "utf8"));

  writeFileSync(path, `${prefix}replaced-tail`);
  utimesSync(path, 1000, 1000);
  const retried = spawnWorker();
  expect(await retried.exited).toBe(1);
  expect(stubCalls()).toBe(0);
  const after = JSON.parse(readFileSync(statePath, "utf8"));
  expect(after.claim.key).toBe(before.claim.key);
  expect(after.claim.files).toEqual(before.claim.files);
});

test("a post-launch crash reuses Plant's durable outcome without relaunch", async () => {
  landFile("mail/a.md", 1000);
  writeIdempotentStub(0, { killParent: true });
  const crashed = spawnWorker();
  expect(await crashed.exited).not.toBe(0);
  expect(stubCalls()).toBe(1);
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeDefined();

  const retried = spawnWorker();
  expect(await retried.exited).toBe(0);
  expect(stubCalls()).toBe(1);
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeUndefined();
});

test("an indeterminate Plant result retains the durable claim", async () => {
  landFile("mail/a.md", 1000);
  writeFileSync(stub, `#!/bin/sh
printf '%s ' "$@" >> "${stubLog}"
cat >> "${stubLog}"
printf '\\n---\\n' >> "${stubLog}"
echo "old untyped failure"
exit 1
`);
  chmodSync(stub, 0o755);
  const spec = { name: "t", watch: "mail/*.md", prompt: () => "go" };
  const result = await door(spec);
  expect(result.code).toBe(1);
  expect(result.detail).toContain("indeterminate");
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeDefined();

  writeFileSync(stub, `#!/bin/sh
printf '%s ' "$@" >> "${stubLog}"
cat >> "${stubLog}"
printf '\\n---\\n' >> "${stubLog}"
echo '{"outcome":"untracked_succeeded","detail":"not recorded"}'
exit 0
`);
  chmodSync(stub, 0o755);
  expect((await door(spec)).detail).toContain("indeterminate");
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeDefined();

  writeStub(0);
  expect((await door(spec)).code).toBe(0);
  expect(stubCalls()).toBe(3);
});

test("breaker pauses a runaway door and stays paused until re-armed", async () => {
  const spec = { name: "t", watch: "mail/*.md", prompt: () => "go", maxFiresPerHour: 2 };
  for (let i = 0; i < 2; i++) {
    landFile(`mail/f${i}.md`, 2000 + i);
    expect((await door(spec)).code).toBe(0);
  }
  landFile("mail/f9.md", 3000);
  const tripped = await door(spec);
  expect(tripped.code).toBe(1);
  expect(tripped.detail).toContain("fire limit");
  expect(stubCalls()).toBe(2);

  const skipped = await door(spec);
  expect(skipped.code).toBe(0);
  expect(skipped.detail).toContain("paused");
  expect(stubCalls()).toBe(2);

  // manual re-arm: delete "paused" from the state file
  const statePath = join(tmp, "state", "t.json");
  const state = JSON.parse(readFileSync(statePath, "utf8"));
  delete state.paused;
  state.fires = [];
  writeFileSync(statePath, JSON.stringify(state));
  expect((await door(spec)).code).toBe(0);
  expect(stubCalls()).toBe(3);
});
