import { afterEach, beforeEach, expect, expectTypeOf, test } from "bun:test";
import {
  chmodSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import {
  agentRun,
  agentRunReceipt,
  type AgentOutcome,
} from "./index.ts";
import {
  setupDoorTest,
  stub,
  stubLog,
  teardownDoorTest,
  writeStub,
} from "./test-support.ts";

beforeEach(setupDoorTest);
afterEach(teardownDoorTest);

test("agentRun preserves the uppercase three-state public contract", async () => {
  writeFileSync(stub, `#!/bin/sh
cat >/dev/null
echo '[agent:agent] succeeded: legacy done'
exit 0
`);
  chmodSync(stub, 0o755);
  const succeeded = await agentRun("p");
  expectTypeOf<AgentOutcome>().toEqualTypeOf<
    "Succeeded" | "Unavailable" | "Failed"
  >();
  expectTypeOf(succeeded).toEqualTypeOf<{
    outcome: AgentOutcome;
    detail: string;
  }>();
  expect(succeeded).toEqual({
    outcome: "Succeeded",
    detail: "[agent:agent] succeeded: legacy done",
  });
  writeStub(75);
  expect((await agentRun("p")).outcome).toBe("Unavailable");
  writeStub(1);
  expect((await agentRun("p")).outcome).toBe("Failed");
});

test("agentRunReceipt accepts only the keyed durable Plant protocol", async () => {
  writeStub(0);
  expect(await agentRunReceipt("p", { idempotencyKey: "door-key" }))
    .toEqual({ outcome: "succeeded", detail: "stub done" });
  writeStub(75);
  expect(await agentRunReceipt("p", { idempotencyKey: "door-key" }))
    .toEqual({ outcome: "retryable", detail: "stub unavailable" });
  writeStub(1);
  expect(await agentRunReceipt("p", { idempotencyKey: "door-key" }))
    .toEqual({ outcome: "failed", detail: "stub done" });
  writeStub(3);
  expect((await agentRunReceipt("p", { idempotencyKey: "door-key" })).outcome)
    .toBe("indeterminate");
  writeFileSync(stub, `#!/bin/sh
cat >/dev/null
echo '{"state":"succeeded","durable":true,"detail":"old protocol"}'
exit 0
`);
  chmodSync(stub, 0o755);
  expect((await agentRunReceipt("p", { idempotencyKey: "door-key" })).outcome)
    .toBe("indeterminate");
  rmSync(stubLog);
  writeStub(0);
  await agentRunReceipt("the prompt", {
    cli: "codex",
    model: "gpt-5.6-sol",
    timeout: "10m",
    idempotencyKey: "door-key",
  });
  const log = readFileSync(stubLog, "utf8");
  expect(log).toContain("agent run --cli codex");
  expect(log).toContain("--model gpt-5.6-sol");
  expect(log).toContain("--timeout 10m");
  expect(log).toContain("--idempotency-key door-key");
  expect(log).toContain("the prompt");
});
