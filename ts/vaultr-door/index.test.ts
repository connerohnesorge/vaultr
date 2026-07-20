import { test, expect, beforeEach, afterEach } from "bun:test";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
  door,
  agentRun,
  setIngestionStatHookForTest,
  setMetadataStatHookForTest,
  setStaleRecoveryHookForTest,
  setStatePublishHookForTest,
} from "./index.ts";

let tmp: string;
let vault: string;
let stub: string;
let stubLog: string;

function writeStub(exitCode: number) {
  const result = exitCode === 0
    ? '{"outcome":"succeeded","detail":"stub done"}'
    : exitCode === 1
      ? '{"outcome":"failed","detail":"stub done"}'
      : exitCode === 75
        ? '{"outcome":"retryable","detail":"stub unavailable"}'
        : '{"outcome":"failed","detail":"bad exit"}';
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
    echo '{"outcome":"succeeded","detail":"stub cached"}'
  else
    echo '{"outcome":"failed","detail":"stub cached"}'
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
  echo '{"outcome":"succeeded","detail":"stub done"}'
else
  echo '{"outcome":"failed","detail":"stub done"}'
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
const { door, setStaleRecoveryHookForTest } = await import(${JSON.stringify(library)});
if (process.env.STALE_RECOVERY_READY && process.env.STALE_RECOVERY_RELEASE) {
  setStaleRecoveryHookForTest(async (owner) => {
    await Bun.write(process.env.STALE_RECOVERY_READY, JSON.stringify(owner));
    while (!(await Bun.file(process.env.STALE_RECOVERY_RELEASE).exists())) {
      await Bun.sleep(5);
    }
  });
}
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

async function waitForFile(path: string): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (existsSync(path)) return;
    await Bun.sleep(5);
  }
  throw new Error(`timed out waiting for ${path}`);
}

function landFile(rel: string, mtimeSec: number) {
  const abs = join(vault, rel);
  mkdirSync(join(abs, ".."), { recursive: true });
  writeFileSync(abs, `content of ${rel}`);
  utimesSync(abs, mtimeSec, mtimeSec);
}

function legacyKey(name: string, from: { mtimeMs: number; path: string }, files: { mtimeMs: number; path: string }[]) {
  return createHash("sha256").update(JSON.stringify({ door: name, from, files })).digest("hex");
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
  setIngestionStatHookForTest();
  setMetadataStatHookForTest();
  setStaleRecoveryHookForTest();
  setStatePublishHookForTest();
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

test("migration publication ENOENT retains the legacy state and never launches", async () => {
  landFile("mail/a.md", 1000);
  const path = join(tmp, "state", "migration.json");
  mkdirSync(join(tmp, "state"), { recursive: true });
  const legacy = JSON.stringify({
    version: 1,
    cursor: { mtimeMs: 0, path: "" },
    fires: [],
  });
  writeFileSync(path, legacy);
  setStatePublishHookForTest(() => {
    throw Object.assign(new Error("injected migration publication ENOENT"), { code: "ENOENT" });
  });

  const result = await door({
    name: "migration",
    watch: "mail/*.md",
    prompt: () => "must not launch",
  });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("injected migration publication ENOENT");
  expect(readFileSync(path, "utf8")).toBe(legacy);
  expect(stubCalls()).toBe(0);
});

test("state control reads reject FIFO, symlink, oversize, swap, and post-open removal", async () => {
  landFile("mail/a.md", 1000);
  const state = join(tmp, "state");
  mkdirSync(state, { recursive: true });
  const fresh = JSON.stringify({
    version: 2,
    frontier: { mtimeMs: 0, seen: [] },
    fires: [],
  });

  const outside = join(tmp, "outside-state.json");
  writeFileSync(outside, fresh);
  symlinkSync(outside, join(state, "state-symlink.json"));
  expect((await door({
    name: "state-symlink",
    watch: "mail/*.md",
    prompt: () => "must not launch",
  })).code).toBe(1);

  expect(Bun.spawnSync(["mkfifo", join(state, "state-fifo.json")]).exitCode).toBe(0);
  const fifo = await Promise.race([
    door({ name: "state-fifo", watch: "mail/*.md", prompt: () => "must not launch" }),
    Bun.sleep(1000).then(() => ({ code: -1 })),
  ]);
  expect(fifo.code).toBe(1);

  writeFileSync(join(state, "state-huge.json"), "x".repeat(1_048_577));
  const huge = await door({
    name: "state-huge",
    watch: "mail/*.md",
    prompt: () => "must not launch",
  });
  expect(huge.code).toBe(1);
  expect(huge.detail).toContain("1048576 byte limit");

  for (const mode of ["swap", "missing"] as const) {
    const name = `state-${mode}`;
    const path = join(state, `${name}.json`);
    writeFileSync(path, fresh);
    let changed = false;
    setMetadataStatHookForTest((relativePath) => {
      if (relativePath !== `${name}.json` || changed) return;
      changed = true;
      if (mode === "swap") {
        renameSync(path, `${path}.opened`);
        symlinkSync(outside, path);
      } else {
        unlinkSync(path);
      }
    });
    const result = await door({
      name,
      watch: "mail/*.md",
      prompt: () => "must not launch",
    });
    expect(result.code).toBe(1);
    expect(changed).toBe(true);
    setMetadataStatHookForTest();
  }
  expect(stubCalls()).toBe(0);
});

test("scalar hwm state migrates durably and conservatively closes its tied timestamp", async () => {
  landFile("mail/tie.md", 1000);
  const path = join(tmp, "state", "hwm.json");
  mkdirSync(join(tmp, "state"), { recursive: true });
  writeFileSync(path, JSON.stringify({ hwm: 1000000, fires: [123] }));
  const spec = { name: "hwm", watch: "mail/*.md", prompt: () => "go" };

  expect((await door(spec)).detail).toBe("no new files");
  const migrated = JSON.parse(readFileSync(path, "utf8"));
  expect(migrated.version).toBe(2);
  expect(migrated.frontier.mtimeMs).toBeGreaterThan(1000000);
  expect(migrated.frontier.seen).toEqual([]);
  expect(stubCalls()).toBe(0);

  landFile("mail/new.md", 1001);
  expect((await door(spec)).code).toBe(0);
  expect(stubCalls()).toBe(1);
});

test("unsafe or non-representable legacy state fails before migration or launch", async () => {
  landFile("mail/a.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  const cases = [
    {
      name: "absolute",
      raw: JSON.stringify({
        version: 1,
        cursor: { mtimeMs: 0, path: "/tmp/outside" },
        fires: [],
      }),
    },
    {
      name: "traversal",
      raw: JSON.stringify({
        version: 1,
        cursor: { mtimeMs: 0, path: "mail/../outside" },
        fires: [],
      }),
    },
    { name: "negative-zero", raw: '{"hwm":-0,"fires":[]}' },
    {
      name: "unclosable",
      raw: JSON.stringify({ hwm: Number.MAX_VALUE, fires: [] }),
    },
  ];

  for (const legacy of cases) {
    const path = join(tmp, "state", `${legacy.name}.json`);
    writeFileSync(path, legacy.raw);
    const result = await door({
      name: legacy.name,
      watch: "mail/*.md",
      prompt: () => "must not launch",
    });
    expect(result.code).toBe(1);
    expect(result.detail).toContain("failed closed");
    expect(readFileSync(path, "utf8")).toBe(legacy.raw);
    expect(stubCalls()).toBe(0);
  }
});

test("v1 cursor and in-progress claim migrate without changing the Plant key", async () => {
  const name = "v1";
  const cursor = { mtimeMs: 1000000, path: "mail/a.md" };
  const files = [{ mtimeMs: 1000000, path: "mail/z.md" }];
  const key = legacyKey(name, cursor, files);
  landFile("mail/z.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  writeFileSync(join(tmp, "state", `${name}.json`), JSON.stringify({
    version: 1,
    cursor,
    fires: [],
    claim: { from: cursor, files, key },
  }));

  const beforeLaunch = await door({
    name,
    watch: "mail/z.md",
    prompt: () => {
      throw new Error("crash before launch");
    },
  });
  expect(beforeLaunch.code).toBe(1);
  const persisted = JSON.parse(readFileSync(join(tmp, "state", `${name}.json`), "utf8"));
  expect(persisted).toMatchObject({
    version: 2,
    frontier: { mtimeMs: cursor.mtimeMs, seen: [], closedThroughPath: cursor.path },
    claim: { key },
  });
  expect(persisted.claim.files[0].sha256).toMatch(/^[0-9a-f]{64}$/);
  expect(persisted.claim.files[0].size).toBe(Buffer.byteLength("content of mail/z.md"));
  expect(stubCalls()).toBe(0);

  expect((await door({ name, watch: "mail/z.md", prompt: () => "go" })).code).toBe(0);
  expect(readFileSync(stubLog, "utf8")).toContain(`--idempotency-key ${key}`);
  const migrated = JSON.parse(readFileSync(join(tmp, "state", `${name}.json`), "utf8"));
  expect(migrated.version).toBe(2);
  expect(migrated.claim).toBeUndefined();
  expect(migrated.frontier).toEqual({
    mtimeMs: 1000000,
    seen: ["mail/z.md"],
    closedThroughPath: "mail/a.md",
  });
});

test("v1 cursor keeps its path boundary and seen set until a newer timestamp", async () => {
  landFile("mail/a.md", 1000);
  landFile("mail/z.md", 1000);
  const path = join(tmp, "state", "v1-tie.json");
  mkdirSync(join(tmp, "state"), { recursive: true });
  writeFileSync(path, JSON.stringify({
    version: 1,
    cursor: { mtimeMs: 1000000, path: "mail/m.md" },
    fires: [],
  }));
  const spec = {
    name: "v1-tie",
    watch: "mail/*.md",
    prompt: (files: any[]) => files.map((file) => file.path).join(" "),
  };

  expect((await door(spec)).code).toBe(0);
  expect(stubCalls()).toBe(1);
  expect(readFileSync(stubLog, "utf8")).toContain("mail/z.md");
  expect(readFileSync(stubLog, "utf8")).not.toContain("mail/a.md");
  expect(JSON.parse(readFileSync(path, "utf8")).frontier).toEqual({
    mtimeMs: 1000000,
    seen: ["mail/z.md"],
    closedThroughPath: "mail/m.md",
  });

  landFile("mail/y.md", 1000);
  expect((await door(spec)).code).toBe(0);
  expect(JSON.parse(readFileSync(path, "utf8")).frontier).toEqual({
    mtimeMs: 1000000,
    seen: ["mail/y.md", "mail/z.md"],
    closedThroughPath: "mail/m.md",
  });

  landFile("mail/new.md", 1001);
  expect((await door(spec)).code).toBe(0);
  expect(JSON.parse(readFileSync(path, "utf8")).frontier).toEqual({
    mtimeMs: 1001000,
    seen: ["mail/new.md"],
  });
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
  let owner: any;
  for (let attempt = 0; attempt < 50 && !owner; attempt++) {
    try {
      owner = JSON.parse(readFileSync(join(tmp, "state", "t.lock"), "utf8"));
    } catch {
      await Bun.sleep(10);
    }
  }
  expect(owner).toMatchObject({ pid: expect.any(Number), token: expect.any(String) });
  const second = spawnWorker();
  const codes = await Promise.all([first.exited, second.exited]);
  expect(codes.sort((a, b) => a - b)).toEqual([0, 75]);
  expect(stubCalls()).toBe(1);
});

test("two processes taking over one stale owner cannot remove the successor lock", async () => {
  landFile("mail/a.md", 1000);
  writeIdempotentStub(0, { delay: 1 });
  mkdirSync(join(tmp, "state"), { recursive: true });
  const exited = Bun.spawn(["/usr/bin/true"]);
  await exited.exited;
  const stale = { pid: exited.pid, token: "stale-owner" };
  const lock = join(tmp, "state", "t.lock");
  writeFileSync(
    lock,
    `${JSON.stringify(stale)}\n`,
  );

  const winnerReady = join(tmp, "winner-ready");
  const winnerRelease = join(tmp, "winner-release");
  const loserReady = join(tmp, "loser-ready");
  const loserRelease = join(tmp, "loser-release");
  const winner = spawnWorker({
    STALE_RECOVERY_READY: winnerReady,
    STALE_RECOVERY_RELEASE: winnerRelease,
  });
  const loser = spawnWorker({
    STALE_RECOVERY_READY: loserReady,
    STALE_RECOVERY_RELEASE: loserRelease,
  });
  await Promise.all([waitForFile(winnerReady), waitForFile(loserReady)]);
  expect(JSON.parse(readFileSync(winnerReady, "utf8"))).toEqual(stale);
  expect(JSON.parse(readFileSync(loserReady, "utf8"))).toEqual(stale);

  writeFileSync(winnerRelease, "");
  let successor: { pid: number; token: string } | undefined;
  for (let attempt = 0; attempt < 200; attempt++) {
    try {
      const owner = JSON.parse(readFileSync(lock, "utf8"));
      if (owner.token !== stale.token) {
        successor = owner;
        break;
      }
    } catch {}
    await Bun.sleep(5);
  }
  expect(successor?.pid).toBe(winner.pid);
  expect(successor?.token).not.toBe(stale.token);

  writeFileSync(loserRelease, "");
  expect(await loser.exited).toBe(75);
  expect(JSON.parse(readFileSync(lock, "utf8"))).toEqual(successor);
  expect(await winner.exited).toBe(0);
  expect(stubCalls()).toBe(1);
});

test("an incomplete legacy lock is retained and fails closed", async () => {
  landFile("mail/a.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  const path = join(tmp, "state", "t.lock");
  writeFileSync(path, "");
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("offline migration");
  expect(readFileSync(path, "utf8")).toBe("");
  expect(stubCalls()).toBe(0);
});

test("an incomplete legacy lock is reread while its publisher finishes", async () => {
  landFile("mail/a.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  const path = join(tmp, "state", "t.lock");
  writeFileSync(path, "");
  setTimeout(() => {
    writeFileSync(path, `${JSON.stringify({ pid: process.pid, token: "legacy-owner" })}\n`);
  }, 5);
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result).toEqual({ code: 75, detail: 'door "t" is already running' });
  expect(JSON.parse(readFileSync(path, "utf8"))).toEqual({
    pid: process.pid,
    token: "legacy-owner",
  });
  expect(stubCalls()).toBe(0);
});

test("lock control reads reject FIFO, symlink, oversize, swap, and post-open removal", async () => {
  landFile("mail/a.md", 1000);
  const state = join(tmp, "state");
  mkdirSync(state, { recursive: true });
  const deadOwner = `${JSON.stringify({ pid: 2_147_483_647, token: "dead" })}\n`;

  const outside = join(tmp, "outside-lock");
  writeFileSync(outside, deadOwner);
  symlinkSync(outside, join(state, "lock-symlink.lock"));
  expect((await door({
    name: "lock-symlink",
    watch: "mail/*.md",
    prompt: () => "must not launch",
  })).code).toBe(1);

  expect(Bun.spawnSync(["mkfifo", join(state, "lock-fifo.lock")]).exitCode).toBe(0);
  const fifo = await Promise.race([
    door({ name: "lock-fifo", watch: "mail/*.md", prompt: () => "must not launch" }),
    Bun.sleep(1000).then(() => ({ code: -1 })),
  ]);
  expect(fifo.code).toBe(1);

  writeFileSync(join(state, "lock-huge.lock"), "x".repeat(1_048_577));
  const huge = await door({
    name: "lock-huge",
    watch: "mail/*.md",
    prompt: () => "must not launch",
  });
  expect(huge.code).toBe(1);
  expect(huge.detail).toContain("1048576 byte limit");

  for (const mode of ["swap", "missing"] as const) {
    const name = `lock-${mode}`;
    const path = join(state, `${name}.lock`);
    writeFileSync(path, deadOwner);
    let changed = false;
    setMetadataStatHookForTest((relativePath) => {
      if (relativePath !== `${name}.lock` || changed) return;
      changed = true;
      if (mode === "swap") {
        renameSync(path, `${path}.opened`);
        symlinkSync(outside, path);
      } else {
        unlinkSync(path);
      }
    });
    const result = await door({
      name,
      watch: "mail/*.md",
      prompt: () => "must not launch",
    });
    expect(result.code).toBe(1);
    expect(changed).toBe(true);
    setMetadataStatHookForTest();
  }
  expect(stubCalls()).toBe(0);
});

test("a crash while preparing owner metadata leaves only an ignorable temp lock", async () => {
  landFile("mail/a.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  writeFileSync(join(tmp, "state", "t.lock.tmp-crashed"), "");
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(0);
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
  setIngestionStatHookForTest((path) => {
    if (path !== "a.md" || ++opens !== 2) return;
    renameSync(join(vault, "mail", "a.md"), join(vault, "mail", "a.opened"));
    symlinkSync(outside, join(vault, "mail", "a.md"));
  });

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
  setIngestionStatHookForTest((path) => {
    if (path !== "a.md" || ++reads !== 1) return;
    writeFileSync(join(vault, "mail", path), "changed of mail/a.md");
    utimesSync(join(vault, "mail", path), 1000, 1000);
  });
  const spec = {
    name: "t",
    watch: "mail/*.md",
    prompt: (files: any[]) => files[0].text,
  };

  const changed = await door(spec);
  expect(changed.code).toBe(1);
  expect(changed.detail).toContain("file changed while reading");
  expect(stubCalls()).toBe(0);

  setIngestionStatHookForTest();
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
  setIngestionStatHookForTest(() => reads++);
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
  expect(await agentRun("p")).toEqual({ outcome: "succeeded", detail: "stub done" });
  writeStub(75);
  expect(await agentRun("p")).toEqual({ outcome: "retryable", detail: "stub unavailable" });
  writeStub(1);
  expect(await agentRun("p")).toEqual({ outcome: "failed", detail: "stub done" });
  writeStub(3);
  expect((await agentRun("p")).outcome).toBe("indeterminate");
  writeFileSync(stub, `#!/bin/sh
cat >/dev/null
echo '{"state":"succeeded","durable":true,"detail":"old protocol"}'
exit 0
`);
  chmodSync(stub, 0o755);
  expect((await agentRun("p")).outcome).toBe("indeterminate");
  rmSync(stubLog);
  writeStub(0);
  await agentRun("the prompt", { cli: "codex", model: "gpt-5.6-sol", timeout: "10m" });
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("agent run --cli codex");
  expect(log).toContain("--model gpt-5.6-sol");
  expect(log).toContain("--timeout 10m");
  expect(log).toContain("the prompt");
});
