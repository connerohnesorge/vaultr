import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { resetTestHooks } from "./test-hooks.ts";

export let tmp: string;
export let vault: string;
export let stub: string;
export let stubLog: string;

export function writeStub(exitCode: number): void {
  const result = exitCode === 0
    ? '{"outcome":"succeeded","detail":"stub done"}'
    : exitCode === 1
      ? '{"outcome":"failed","detail":"stub done"}'
      : exitCode === 75
        ? '{"outcome":"retryable","detail":"stub unavailable"}'
        : '{"outcome":"failed","detail":"bad exit"}';
  writeFileSync(
    stub,
    `#!/bin/sh\nprintf '%s ' "$@" >> "${stubLog}"\ncat >> "${stubLog}"\nprintf '\\n---\\n' >> "${stubLog}"\nprintf '%s\\n' '${result}'\nexit ${exitCode}\n`,
  );
  chmodSync(stub, 0o755);
}

export function writeIdempotentStub(
  exitCode: number,
  opts: { delay?: number; killParent?: boolean } = {},
): void {
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

export function stubCalls(): number {
  try {
    return readFileSync(stubLog, "utf8")
      .split("---")
      .filter((value) => value.trim())
      .length;
  } catch {
    return 0;
  }
}

function writeWorker(): string {
  const worker = join(tmp, "door-worker.ts");
  const library = pathToFileURL(resolve(import.meta.dir, "index.ts")).href;
  const hooks = pathToFileURL(resolve(import.meta.dir, "test-hooks.ts")).href;
  writeFileSync(worker, `
const { door } = await import(${JSON.stringify(library)});
const { testHooks } = await import(${JSON.stringify(hooks)});
if (process.env.STATE_PUBLISH_READY && process.env.STATE_PUBLISH_RELEASE) {
  const {
    existsSync,
    symlinkSync,
    unlinkSync,
    utimesSync,
    writeFileSync,
  } = await import("node:fs");
  const sleeper = new Int32Array(new SharedArrayBuffer(4));
  const mutate = process.env.FLIP_STATE_ALIAS
    && process.env.STATE_SUCCESSOR
    && process.env.LATE_FILE
    ? () => {
      unlinkSync(process.env.FLIP_STATE_ALIAS);
      symlinkSync(process.env.STATE_SUCCESSOR, process.env.FLIP_STATE_ALIAS);
      writeFileSync(process.env.LATE_FILE, "late arrival");
      utimesSync(process.env.LATE_FILE, 2000, 2000);
    }
    : () => {};
  testHooks.beforeStatePublish = () => {
    mutate();
    writeFileSync(process.env.STATE_PUBLISH_READY, "");
    while (!existsSync(process.env.STATE_PUBLISH_RELEASE)) {
      Atomics.wait(sleeper, 0, 0, 5);
    }
  };
}
if (process.env.STALE_RECOVERY_READY && process.env.STALE_RECOVERY_RELEASE) {
  testHooks.beforeStaleRecovery = async (owner) => {
    await Bun.write(process.env.STALE_RECOVERY_READY, JSON.stringify(owner));
    while (!(await Bun.file(process.env.STALE_RECOVERY_RELEASE).exists())) {
      await Bun.sleep(5);
    }
  };
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

export function spawnWorker(extraEnv: Record<string, string> = {}) {
  return Bun.spawn([process.execPath, writeWorker()], {
    env: { ...process.env, ...extraEnv },
    stdout: "pipe",
    stderr: "pipe",
  });
}

/**
 * Runs a trivial process to completion and returns its pid, which is then a pid
 * that genuinely belonged to a real, now-exited process. Spawns the running Bun
 * binary rather than an absolute FHS path, which does not exist on NixOS.
 */
export async function spawnExitedProcess(): Promise<number> {
  const child = Bun.spawn([process.execPath, "-e", ""], {
    stdout: "ignore",
    stderr: "ignore",
  });
  const pid = child.pid;
  const code = await child.exited;
  if (code !== 0) {
    throw new Error(`expected a clean exit from ${process.execPath}, got ${code}`);
  }
  return pid;
}

export async function waitForFile(path: string): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (existsSync(path)) return;
    await Bun.sleep(5);
  }
  throw new Error(`timed out waiting for ${path}`);
}

export function landFile(rel: string, mtimeSec: number): void {
  const abs = join(vault, rel);
  mkdirSync(join(abs, ".."), { recursive: true });
  writeFileSync(abs, `content of ${rel}`);
  utimesSync(abs, mtimeSec, mtimeSec);
}

export function legacyKey(
  name: string,
  from: { mtimeMs: number; path: string },
  files: { mtimeMs: number; path: string }[],
): string {
  return createHash("sha256")
    .update(JSON.stringify({ door: name, from, files }))
    .digest("hex");
}

export function setupDoorTest(): void {
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
}

export function teardownDoorTest(): void {
  resetTestHooks();
  rmSync(tmp, { recursive: true, force: true });
}
