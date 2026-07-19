import { test, expect, beforeEach } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, chmodSync, readFileSync, utimesSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { door, agentRun } from "./index.ts";

let tmp: string;
let vault: string;
let stub: string;
let stubLog: string;

function writeStub(exitCode: number) {
  writeFileSync(stub, `#!/bin/sh\nprintf '%s ' "$@" >> "${stubLog}"\ncat >> "${stubLog}"\nprintf '\\n---\\n' >> "${stubLog}"\necho "stub done"\nexit ${exitCode}\n`);
  chmodSync(stub, 0o755);
}

function stubCalls(): number {
  try {
    return readFileSync(stubLog, "utf8").split("---").filter((s) => s.trim()).length;
  } catch {
    return 0;
  }
}

function landFile(rel: string, mtimeSec: number) {
  const abs = join(vault, rel);
  mkdirSync(join(abs, ".."), { recursive: true });
  writeFileSync(abs, `content of ${rel}`);
  utimesSync(abs, mtimeSec, mtimeSec);
}

beforeEach(() => {
  tmp = mkdtempSync(join(tmpdir(), "door-test-"));
  vault = join(tmp, "vault");
  stub = join(tmp, "plant-stub");
  stubLog = join(tmp, "stub.log");
  mkdirSync(join(vault, "mail"), { recursive: true });
  process.env.DOOR_VAULT_ROOT = vault;
  process.env.DOOR_STATE_DIR = join(tmp, "state");
  process.env.PLANT_BIN = stub;
  delete process.env.DOOR_ROOTS;
  writeStub(0);
});

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

  const second = await door(spec);
  expect(second.code).toBe(0);
  expect(second.detail).toBe("no new files");
  expect(stubCalls()).toBe(1);
});

test("Unavailable does not advance the fence; files fire on the next run", async () => {
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

test("failed launch keeps the batch for retry", async () => {
  landFile("mail/a.md", 1000);
  const spec = { name: "t", watch: "mail/*.md", prompt: () => "go" };
  writeStub(1);
  expect((await door(spec)).code).toBe(1);
  writeStub(0);
  expect((await door(spec)).detail).toContain("fired on 1 file");
});

test("non-ingestion watch roots are rejected before any launch", async () => {
  mkdirSync(join(vault, "learnings"), { recursive: true });
  landFile("learnings/x.md", 1000);
  const res = await door({ name: "t", watch: "learnings/**/*.md", prompt: () => "go" });
  expect(res.code).toBe(1);
  expect(res.detail).toContain("rejected");
  expect(stubCalls()).toBe(0);
});

test("filter narrows the batch, including by content", async () => {
  landFile("mail/keep.md", 1000);
  landFile("mail/skip.md", 1001);
  await door({ name: "t", watch: "mail/*.md", filter: (f) => f.text.includes("of mail/keep"), prompt: (fs) => fs.map((f) => f.path).join(" ") });
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("keep.md");
  expect(log).not.toContain("skip.md");
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

test("agentRun maps plant exit codes to typed outcomes", async () => {
  writeStub(0);
  expect((await agentRun("p")).outcome).toBe("Succeeded");
  writeStub(75);
  expect((await agentRun("p")).outcome).toBe("Unavailable");
  writeStub(3);
  expect((await agentRun("p")).outcome).toBe("Failed");
  rmSync(stubLog);
  writeStub(0);
  await agentRun("the prompt", { cli: "codex", model: "gpt-5.6-sol", timeout: "10m" });
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("agent run --cli codex");
  expect(log).toContain("--model gpt-5.6-sol");
  expect(log).toContain("--timeout 10m");
  expect(log).toContain("the prompt");
});
