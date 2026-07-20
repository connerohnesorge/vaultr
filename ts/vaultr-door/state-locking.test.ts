import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { door } from "./index.ts";
import { acquireLock } from "./state.ts";
import {
  landFile,
  legacyKey,
  setupDoorTest,
  spawnWorker,
  stubCalls,
  stubLog,
  teardownDoorTest,
  tmp,
  vault,
  waitForFile,
  writeIdempotentStub,
} from "./test-support.ts";
import { testHooks } from "./test-hooks.ts";

beforeEach(setupDoorTest);
afterEach(teardownDoorTest);

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
  testHooks.beforeStatePublish = () => {
    throw Object.assign(new Error("injected migration publication ENOENT"), { code: "ENOENT" });
  };

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
    testHooks.afterMetadataStat = (relativePath) => {
      if (relativePath !== `${name}.json` || changed) return;
      changed = true;
      if (mode === "swap") {
        renameSync(path, `${path}.opened`);
        symlinkSync(outside, path);
      } else {
        unlinkSync(path);
      }
    };
    const result = await door({
      name,
      watch: "mail/*.md",
      prompt: () => "must not launch",
    });
    expect(result.code).toBe(1);
    expect(changed).toBe(true);
    delete testHooks.afterMetadataStat;
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
  for (let attempt = 0; attempt < 200 && !owner; attempt++) {
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

test("claim publication rejects an alias successor without splitting launches", async () => {
  landFile("mail/a.md", 1000);
  const oldBase = join(tmp, "old-base");
  const newBase = join(tmp, "new-base");
  const alias = join(tmp, "state-alias");
  mkdirSync(join(oldBase, "doors"), { recursive: true });
  mkdirSync(join(newBase, "doors"), { recursive: true });
  symlinkSync(oldBase, alias);
  process.env.DOOR_STATE_DIR = join(alias, "doors");

  const firstReady = join(tmp, "first-state-publish-ready");
  const firstRelease = join(tmp, "first-state-publish-release");
  const first = spawnWorker({
    FLIP_STATE_ALIAS: alias,
    STATE_SUCCESSOR: newBase,
    LATE_FILE: join(vault, "mail", "late.md"),
    STATE_PUBLISH_READY: firstReady,
    STATE_PUBLISH_RELEASE: firstRelease,
  });
  await waitForFile(firstReady);

  const successorReady = join(tmp, "successor-state-publish-ready");
  const successorRelease = join(tmp, "successor-state-publish-release");
  const successor = spawnWorker({
    STATE_PUBLISH_READY: successorReady,
    STATE_PUBLISH_RELEASE: successorRelease,
  });
  await waitForFile(successorReady);

  writeFileSync(firstRelease, "");
  expect(await first.exited).toBe(1);
  expect(stubCalls()).toBe(0);
  unlinkSync(alias);
  symlinkSync(oldBase, alias);
  writeFileSync(successorRelease, "");
  expect(await successor.exited).toBe(1);
  expect(existsSync(join(newBase, "doors", "t.json"))).toBe(false);

  writeIdempotentStub(0, { delay: 0.3 });
  const winner = spawnWorker();
  await waitForFile(join(oldBase, "doors", "t.lock"));
  const loser = spawnWorker();
  expect(
    (await Promise.all([winner.exited, loser.exited])).sort((a, b) => a - b),
  ).toEqual([0, 75]);
  expect(stubCalls()).toBe(1);
  expect(
    JSON.parse(readFileSync(join(oldBase, "doors", "t.json"), "utf8"))
      .frontier.seen,
  ).toEqual(["mail/late.md"]);
  expect(existsSync(join(newBase, "doors", "t.json"))).toBe(false);
});

test("two contenders retaining one stale inode cannot remove its successor lock", async () => {
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
  expect(existsSync(`${lock}.recovery`)).toBe(false);
});

test("a live successor swapped in after guarded owner parse is retained", async () => {
  landFile("mail/a.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  const exited = Bun.spawn(["/usr/bin/true"]);
  await exited.exited;
  const stale = { pid: exited.pid, token: "stale-owner" };
  const successor = { pid: process.pid, token: "live-successor" };
  const lock = join(tmp, "state", "t.lock");
  writeFileSync(lock, `${JSON.stringify(stale)}\n`);
  let swapped = false;
  testHooks.afterStaleOwnerRead = () => {
    renameSync(lock, `${lock}.stale`);
    writeFileSync(lock, `${JSON.stringify(successor)}\n`);
    swapped = true;
  };

  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "must not launch" });
  expect(result).toEqual({ code: 75, detail: 'door "t" is already running' });
  expect(swapped).toBe(true);
  expect(JSON.parse(readFileSync(lock, "utf8"))).toEqual(successor);
  expect(JSON.parse(readFileSync(`${lock}.stale`, "utf8"))).toEqual(stale);
  expect(stubCalls()).toBe(0);
});

test("retargeting an intermediate state alias cannot redirect stale unlink", async () => {
  landFile("mail/a.md", 1000);
  const oldBase = join(tmp, "old-base");
  const newBase = join(tmp, "new-base");
  const alias = join(tmp, "state-alias");
  mkdirSync(join(oldBase, "doors"), { recursive: true });
  mkdirSync(join(newBase, "doors"), { recursive: true });
  symlinkSync(oldBase, alias);
  process.env.DOOR_STATE_DIR = join(alias, "doors");

  const exited = Bun.spawn(["/usr/bin/true"]);
  await exited.exited;
  const stale = { pid: exited.pid, token: "stale-owner" };
  const successor = { pid: process.pid, token: "new-root-successor" };
  const oldLock = join(oldBase, "doors", "t.lock");
  const successorLock = join(newBase, "doors", "t.lock");
  writeFileSync(oldLock, `${JSON.stringify(stale)}\n`);
  writeFileSync(successorLock, `${JSON.stringify(successor)}\n`);
  testHooks.afterStaleOwnerRead = () => {
    unlinkSync(alias);
    symlinkSync(newBase, alias);
  };

  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "must not launch" });
  expect(result.code).toBe(1);
  expect(result.detail).toContain("configured directory changed");
  expect(existsSync(oldLock)).toBe(false);
  expect(JSON.parse(readFileSync(successorLock, "utf8"))).toEqual(successor);
  expect(stubCalls()).toBe(0);
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

test("a contender retries when the observed owner releases its published lock", async () => {
  const name = "release-race";
  const first = await acquireLock(name);
  if (!first) throw new Error("first lease not acquired");
  const path = join(tmp, "state", `${name}.lock`);
  const original = JSON.parse(readFileSync(path, "utf8"));
  let released = false;
  testHooks.afterMetadataStat = (relativePath) => {
    if (relativePath !== `${name}.lock` || released) return;
    released = true;
    first.release();
  };

  const contender = await acquireLock(name);
  if (!contender) throw new Error("contender lease not acquired");
  expect(released).toBe(true);
  const successor = JSON.parse(readFileSync(path, "utf8"));
  expect(successor.token).not.toBe(original.token);
  expect(await acquireLock(name)).toBeUndefined();
  expect(JSON.parse(readFileSync(path, "utf8"))).toEqual(successor);

  delete testHooks.afterMetadataStat;
  contender.release();
  expect(existsSync(path)).toBe(false);
});

test("lock control reads reject FIFO, symlink, oversize, and swap but retry removal", async () => {
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

  mkdirSync(join(state, "lock-directory.lock"));
  expect((await door({
    name: "lock-directory",
    watch: "mail/*.md",
    prompt: () => "must not launch",
  })).code).toBe(1);

  for (const mode of ["swap", "missing"] as const) {
    const name = `lock-${mode}`;
    const path = join(state, `${name}.lock`);
    writeFileSync(path, deadOwner);
    let changed = false;
    testHooks.afterMetadataStat = (relativePath) => {
      if (relativePath !== `${name}.lock` || changed) return;
      changed = true;
      if (mode === "swap") {
        renameSync(path, `${path}.opened`);
        symlinkSync(outside, path);
      } else {
        unlinkSync(path);
      }
    };
    const result = await door({
      name,
      watch: "mail/*.md",
      prompt: () => "must not launch",
    });
    expect(result.code).toBe(mode === "missing" ? 0 : 1);
    expect(changed).toBe(true);
    delete testHooks.afterMetadataStat;
  }
  expect(stubCalls()).toBe(1);
});

test("a crash while preparing owner metadata leaves only an ignorable temp lock", async () => {
  landFile("mail/a.md", 1000);
  mkdirSync(join(tmp, "state"), { recursive: true });
  writeFileSync(join(tmp, "state", "t.lock.tmp-crashed"), "");
  const result = await door({ name: "t", watch: "mail/*.md", prompt: () => "go" });
  expect(result.code).toBe(0);
  expect(stubCalls()).toBe(1);
});
