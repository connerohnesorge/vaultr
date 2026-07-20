const plantBin = () => process.env.PLANT_BIN ?? "plant";

export type AgentOutcome = "Succeeded" | "Unavailable" | "Failed";

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
}

export interface AgentRunReceiptOpts extends AgentOpts {
  idempotencyKey: string;
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

function lastLine(text: string): string | undefined {
  return text.split("\n").map((line) => line.trim()).filter(Boolean).pop();
}

async function invokePlant(
  prompt: string,
  opts: AgentOpts,
  idempotencyKey?: string,
) {
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
  ] as const) {
    if (value) argv.push(flag, value);
  }
  if (idempotencyKey) argv.push("--idempotency-key", idempotencyKey);
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
  return { out, err, code };
}

/** Legacy typed client: exit 0/75/other maps to uppercase outcomes. */
export async function agentRun(
  prompt: string,
  opts: AgentOpts = {},
): Promise<{ outcome: AgentOutcome; detail: string }> {
  const { out, err, code } = await invokePlant(prompt, opts);
  const outcome: AgentOutcome = code === 0
    ? "Succeeded"
    : code === 75 ? "Unavailable" : "Failed";
  return {
    outcome,
    detail: lastLine(out) ?? lastLine(err) ?? "no output",
  };
}

/** Keyed durable protocol client for Door claims. */
export async function agentRunReceipt(
  prompt: string,
  opts: AgentRunReceiptOpts,
): Promise<AgentRunReceipt> {
  const { idempotencyKey, ...agentOpts } = opts;
  const { out, err, code } = await invokePlant(
    prompt,
    agentOpts,
    idempotencyKey,
  );
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
