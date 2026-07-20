import { test, expect, beforeEach, afterEach } from "bun:test";
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { door, agentRun } from "./index.ts";

let tmp: string;
let vault: string;
let stub: string;
let stubLog: string;

function writeStub(exitCode: number) {
  const result = exitCode === 0
    ? '{"type":"plant.agent.run","version":1,"state":"succeeded","durable":true,"detail":"stub done"}'
    : exitCode === 1
      ? '{"type":"plant.agent.run","version":1,"state":"failed","durable":true,"detail":"stub done"}'
      : exitCode === 75
        ? '{"type":"plant.agent.run","version":1,"state":"retryable","durable":false,"detail":"stub unavailable"}'
        : '{"type":"plant.agent.run","version":1,"state":"failed","durable":true,"detail":"bad exit"}';
  writeFileSync(stub, `#!/bin/sh\nprintf '%s ' "$@" >> "${stubLog}"\ncat >> "${stubLog}"\nprintf '\\n---\\n' >> "${stubLog}"\nprintf '%s\\n' '${result}'\nexit ${exitCode}\n`);
  chmodSync(stub, 0o755);
}

function writeIdempotentStub(exitCode: number, opts: { delay?: number; killParent?: boolean } = {}) {
  const outcomes = join(tmp, "stub-outcomes");
  writeFileSync(stub, `#!/bin/sh
key=
previous=
for arg in "$@"; do
  if [ "$previous" = "--idempotency-key" ]; then key="$arg"; fi
  previous="$arg"
done
mkdir -p "${outcomes}"
if [ -n "$key" ] && [ -f "${outcomes}/$key" ]; then
  cat >/dev/null
  code="$(cat "${outcomes}/$key")"
  if [ "$code" = 0 ]; then
    echo '{"type":"plant.agent.run","version":1,"state":"succeeded","durable":true,"detail":"stub cached"}'
  else
    echo '{"type":"plant.agent.run","version":1,"state":"failed","durable":true,"detail":"stub cached"}'
  fi
  exit "$code"
fi
printf '%s ' "$@" >> "${stubLog}"
cat >> "${stubLog}"
printf '\\n---\\n' >> "${stubLog}"
${opts.delay ? `sleep ${opts.delay}` : ""}
if [ -n "$key" ]; then
  printf '%s\\n' "${exitCode}" > "${outcomes}/$key.tmp"
  mv "${outcomes}/$key.tmp" "${outcomes}/$key"
fi
${opts.killParent ? 'kill -KILL "$PPID"' : ""}
if [ "${exitCode}" = 0 ]; then
  echo '{"type":"plant.agent.run","version":1,"state":"succeeded","durable":true,"detail":"stub done"}'
else
  echo '{"type":"plant.agent.run","version":1,"state":"failed","durable":true,"detail":"stub done"}'
fi
exit ${exitCode}
`);
  chmodSync(stub, 0o755);
}

function stubCalls(): number {
  try {
    return readFileSync(stubLog, "utf8").split("---").filter((s) => s.trim()).length;
  } catch {
    return 0;
  }
}

function writeWorker(): string {
  const worker = join(tmp, "door-worker.ts");
  const library = pathToFileURL(resolve(import.meta.dir, "index.ts")).href;
  writeFileSync(worker, `
const { door } = await import(${JSON.stringify(library)});
const result = await door({
  name: "t",
  watch: "mail/*.md",
  prompt: () => {
    if (process.env.CRASH_IN_PROMPT === "1") process.exit(86);
    return "go";
  },
});
console.log(JSON.stringify(result));
process.exit(result.code);
`);
  return worker;
}

function spawnWorker(extraEnv: Record<string, string> = {}) {
  return Bun.spawn([process.execPath, writeWorker()], {
    env: { ...process.env, ...extraEnv },
    stdout: "pipe",
    stderr: "pipe",
  });
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

afterEach(() => {
  rmSync(tmp, { recursive: true, force: true });
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

test("corrupt state fails closed without replacement or launch", async () => {
  landFile("mail/a.md", 1000);
  const path = join(tmp, "state", "t.json");
  mkdirSync(join(tmp, "state"), { recursive: true });
  writeFileSync(path, "{");
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("failed closed");
  expect(readFileSync(path, "utf8")).toBe("{");
  expect(stubCalls()).toBe(0);
});

test("a later lower-sorting path at the frontier timestamp is not missed", async () => {
  const spec = { name: "t", watch: "mail/*.md", prompt: (files: any[]) => files.map((file) => file.path).join(" ") };
  landFile("mail/z.md", 1000);
  expect((await door(spec)).code).toBe(0);
  landFile("mail/a.md", 1000);
  expect((await door(spec)).code).toBe(0);
  expect(stubCalls()).toBe(2);
  expect(readFileSync(stubLog, "utf8")).toContain("mail/a.md");
  const state = JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8"));
  expect(state.frontier).toEqual({ mtimeMs: 1000000, seen: ["mail/a.md", "mail/z.md"] });
});

test("concurrent door processes launch one batch once", async () => {
  landFile("mail/a.md", 1000);
  writeIdempotentStub(0, { delay: 0.3 });
  const first = spawnWorker();
  const second = spawnWorker();
  const codes = await Promise.all([first.exited, second.exited]);
  expect(codes.sort((a, b) => a - b)).toEqual([0, 75]);
  expect(stubCalls()).toBe(1);
});

test("a pre-launch crash resumes the persisted ordered claim and key", async () => {
  landFile("mail/a.md", 1000);
  const crashed = spawnWorker({ CRASH_IN_PROMPT: "1" });
  expect(await crashed.exited).toBe(86);
  expect(stubCalls()).toBe(0);
  const claimed = JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8"));
  expect(claimed.claim.files).toEqual([{ mtimeMs: claimed.claim.files[0].mtimeMs, path: "mail/a.md" }]);
  expect(claimed.claim.key).toHaveLength(64);

  const retried = spawnWorker();
  expect(await retried.exited).toBe(0);
  expect(stubCalls()).toBe(1);
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
echo '{"type":"plant.agent.run","version":1,"state":"succeeded","durable":false,"detail":"not recorded"}'
exit 0
`);
  chmodSync(stub, 0o755);
  expect((await door(spec)).detail).toContain("indeterminate");
  expect(JSON.parse(readFileSync(join(tmp, "state", "t.json"), "utf8")).claim).toBeDefined();

  writeStub(0);
  expect((await door(spec)).code).toBe(0);
  expect(stubCalls()).toBe(3);
});

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
  expect(result.detail).toContain("symlink escapes ingestion root");
  expect(stubCalls()).toBe(0);
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

test("agentRun accepts only a valid machine-readable Plant result", async () => {
  writeStub(0);
  expect(await agentRun("p")).toMatchObject({ state: "Succeeded", durable: true });
  writeStub(75);
  expect(await agentRun("p")).toMatchObject({ state: "Retryable", durable: false });
  writeStub(1);
  expect(await agentRun("p")).toMatchObject({ state: "Failed", durable: true });
  writeStub(3);
  expect(await agentRun("p")).toMatchObject({ state: "Indeterminate", durable: false });
  rmSync(stubLog);
  writeStub(0);
  await agentRun("the prompt", { cli: "codex", model: "gpt-5.6-sol", timeout: "10m" });
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("agent run --cli codex");
  expect(log).toContain("--model gpt-5.6-sol");
  expect(log).toContain("--timeout 10m");
  expect(log).toContain("the prompt");
});
