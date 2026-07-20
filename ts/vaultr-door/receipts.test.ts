import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  chmodSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { agentRun } from "./index.ts";
import {
  setupDoorTest,
  stub,
  stubLog,
  teardownDoorTest,
  writeStub,
} from "./test-support.ts";

beforeEach(setupDoorTest);
afterEach(teardownDoorTest);

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
