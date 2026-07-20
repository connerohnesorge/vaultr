const plantBin = () => process.env.PLANT_BIN ?? "plant";

export type AgentRunReceipt =
  | { outcome: "succeeded"; detail: string }
  | { outcome: "failed"; detail: string }
  | { outcome: "untracked_succeeded"; detail: string }
  | { outcome: "untracked_failed"; detail: string }
  | { outcome: "retryable"; detail: string }
  | { outcome: "indeterminate"; detail: string };

export interface AgentOpts {
  cli?: "claude" | "codex";
  label?: string;
  model?: string;
  args?: string;
  cwd?: string;
  timeout?: string;
  cleanup?: "never" | "always" | "on-success";
  idempotencyKey?: string;
}

function receiptExitCode(receipt: AgentRunReceipt): 0 | 1 | 75 {
  if (receipt.outcome === "succeeded" || receipt.outcome === "untracked_succeeded") return 0;
  return receipt.outcome === "retryable" ? 75 : 1;
}

export function receiptDurable(receipt: AgentRunReceipt): boolean {
  return receipt.outcome === "succeeded" || receipt.outcome === "failed";
}

function parseReceipt(value: unknown): AgentRunReceipt | undefined {
  const receipt = value as Record<string, unknown>;
  if (!receipt || typeof receipt !== "object" || Array.isArray(receipt)
    || Object.keys(receipt).some((key) => key !== "outcome" && key !== "detail")
    || typeof receipt.detail !== "string"
    || !["succeeded", "failed", "untracked_succeeded", "untracked_failed", "retryable", "indeterminate"].includes(receipt.outcome as string)) {
    return undefined;
  }
  return receipt as AgentRunReceipt;
}

/** Typed client over `plant agent run`, the only sanctioned agent launcher. */
export async function agentRun(
  prompt: string,
  opts: AgentOpts = {},
): Promise<AgentRunReceipt> {
  const argv = [
    plantBin(),
    "agent",
    "run",
    "--cli",
    opts.cli ?? "claude",
    "--label",
    opts.label ?? "agent",
  ];
  for (const [flag, value] of [
    ["--model", opts.model],
    ["--args", opts.args],
    ["--cwd", opts.cwd],
    ["--timeout", opts.timeout],
    ["--cleanup", opts.cleanup],
    ["--idempotency-key", opts.idempotencyKey],
  ] as const) {
    if (value) argv.push(flag, value);
  }
  const proc = Bun.spawn(argv, {
    stdin: new TextEncoder().encode(prompt),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [out, err, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  const lastLine = (text: string) =>
    text.split("\n").map((line) => line.trim()).filter(Boolean).pop();
  const fallback = lastLine(err) ?? lastLine(out) ?? "no output";
  let receipt: AgentRunReceipt | undefined;
  try {
    receipt = parseReceipt(JSON.parse(lastLine(out) ?? ""));
  } catch {}
  if (!receipt || receiptExitCode(receipt) !== code) {
    return {
      outcome: "indeterminate",
      detail: `invalid Plant receipt (exit ${code}): ${fallback}`,
    };
  }
  return receipt;
}
