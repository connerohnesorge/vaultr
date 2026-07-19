// Door library — a door is a Plant job (door-<name>.<interval>.ts) that watches
// ingested Vault Content and launches one Herdr agent session per batch of new
// files, via `plant agent run` (never a bare CLI). The library owns the fragile
// parts once: high-water dedup fence, ingestion-root allowlist, rolling-window
// fire breaker, typed launch outcome. Spec: .rocks/specs/vault-doors/spec.md.

import { basename } from "node:path";
import { mkdirSync, statSync, readFileSync } from "node:fs";

const HOME = process.env.HOME ?? "";
const vaultRoot = () => process.env.DOOR_VAULT_ROOT ?? `${HOME}/.dotfiles/vault`;
const stateDir = () => process.env.DOOR_STATE_DIR ?? `${HOME}/.local/state/plant/doors`;
const plantBin = () => process.env.PLANT_BIN ?? "plant";
// Ingestion-only roots (written by sync jobs, never by agents) — a door cannot
// watch agent-written Vault Content, so it cannot trigger off its own output.
const allowedRoots = () => (process.env.DOOR_ROOTS ?? "mail,teams,tickets").split(",").map((r) => r.trim()).filter(Boolean);

const WINDOW_MS = 3_600_000;

export type AgentOutcome = "Succeeded" | "Unavailable" | "Failed";

export interface AgentOpts {
  cli?: "claude" | "codex";
  label?: string;
  model?: string;
  args?: string;
  cwd?: string;
  timeout?: string; // plant duration, e.g. "45m"
  cleanup?: "never" | "always" | "on-success";
}

/** Typed client over `plant agent run` — the only sanctioned way to drive an
 *  agent pane. Exit contract mirrors plant: 0 Succeeded, 75 Unavailable, else Failed. */
export async function agentRun(prompt: string, opts: AgentOpts = {}): Promise<{ outcome: AgentOutcome; detail: string }> {
  const argv = [plantBin(), "agent", "run", "--cli", opts.cli ?? "claude", "--label", opts.label ?? "agent"];
  for (const [flag, v] of [["--model", opts.model], ["--args", opts.args], ["--cwd", opts.cwd], ["--timeout", opts.timeout], ["--cleanup", opts.cleanup]] as const) {
    if (v) argv.push(flag, v);
  }
  const proc = Bun.spawn(argv, { stdin: new TextEncoder().encode(prompt), stdout: "pipe", stderr: "pipe" });
  const [out, err, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  const lastLine = (s: string) => s.split("\n").map((l) => l.trim()).filter(Boolean).pop();
  const detail = lastLine(out) ?? lastLine(err) ?? "no output";
  const outcome: AgentOutcome = code === 0 ? "Succeeded" : code === 75 ? "Unavailable" : "Failed";
  return { outcome, detail };
}

export interface DoorFile {
  path: string; // vault-root-relative
  abs: string;
  mtimeMs: number;
  text: string; // file content (first 64 KiB), for content filters and prompts
}

export interface DoorSpec {
  /** Defaults to the job filename: door-oncall.30m.ts -> "oncall". */
  name?: string;
  /** Glob relative to the vault content root; first segment must be an allowlisted ingestion root. */
  watch: string;
  filter?: (f: DoorFile) => boolean;
  prompt: (files: DoorFile[]) => string;
  agent?: AgentOpts;
  /** Rolling-window breaker; exceeding this pauses the door until manual re-arm. */
  maxFiresPerHour?: number;
}

export interface DoorResult {
  code: 0 | 1 | 75; // Plant job exit contract: 0 success, 75 retry-no-record, else failed
  detail: string;
}

interface DoorState {
  hwm: number; // max mtimeMs already fired on
  fires: number[]; // epoch-ms of launches in/near the rolling window
  paused?: string; // reason; presence = paused until manually removed
}

function statePath(name: string): string {
  return `${stateDir()}/${name}.json`;
}

async function loadState(name: string): Promise<DoorState> {
  try {
    return await Bun.file(statePath(name)).json();
  } catch {
    return { hwm: 0, fires: [] };
  }
}

async function saveState(name: string, s: DoorState): Promise<void> {
  mkdirSync(stateDir(), { recursive: true });
  await Bun.write(statePath(name), JSON.stringify(s));
}

function doorNameFromScript(): string {
  // "door-oncall.30m.ts" -> "oncall"
  const first = basename(Bun.main).split(".")[0] ?? "door";
  return first.startsWith("door-") ? first.slice(5) : first;
}

export async function door(spec: DoorSpec): Promise<DoorResult> {
  const name = spec.name ?? doorNameFromScript();
  const state = await loadState(name);

  if (state.paused) {
    return { code: 0, detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath(name)}` };
  }

  const root = spec.watch.split("/")[0] ?? "";
  if (/[*?[\]]/.test(root) || !allowedRoots().includes(root)) {
    return { code: 1, detail: `watch "${spec.watch}" rejected: first segment must be an ingestion root (${allowedRoots().join(", ")})` };
  }

  const files: DoorFile[] = [];
  for (const rel of new Bun.Glob(spec.watch).scanSync({ cwd: vaultRoot() })) {
    const abs = `${vaultRoot()}/${rel}`;
    let mtimeMs: number;
    try {
      mtimeMs = statSync(abs).mtimeMs;
    } catch {
      continue; // raced away mid-scan
    }
    // ponytail: strict > can miss a file written in the same ms as the previous
    // batch's max — irrelevant at sync cadences; revisit only if doors go sub-second
    if (mtimeMs > state.hwm) {
      const text = readFileSync(abs, "utf8").slice(0, 65536);
      files.push({ path: rel, abs, mtimeMs, text });
    }
  }
  const matched = (spec.filter ? files.filter(spec.filter) : files).sort((a, b) => a.mtimeMs - b.mtimeMs);
  if (matched.length === 0) return { code: 0, detail: "no new files" };

  const now = Date.now();
  state.fires = state.fires.filter((t) => now - t < WINDOW_MS);
  const max = spec.maxFiresPerHour ?? 4;
  if (state.fires.length >= max) {
    state.paused = `fire limit hit (${state.fires.length} fires in 1h, max ${max})`;
    await saveState(name, state);
    return { code: 1, detail: `paused: ${state.paused} — re-arm by deleting "paused" in ${statePath(name)}` };
  }

  const { outcome, detail } = await agentRun(spec.prompt(matched), { label: `door-${name}`, ...spec.agent });
  if (outcome === "Unavailable") {
    // no fence advance, no fire recorded: same files are eligible next run
    return { code: 75, detail: `herdr unavailable, ${matched.length} file(s) held` };
  }
  state.fires.push(now);
  if (outcome === "Succeeded") {
    state.hwm = matched[matched.length - 1]!.mtimeMs; // advance only after the outcome is known
  }
  await saveState(name, state);
  return outcome === "Succeeded"
    ? { code: 0, detail: `fired on ${matched.length} file(s): ${detail}` }
    : { code: 1, detail: `agent failed (batch retries next run): ${detail}` };
}

/** Entry point for door job scripts: print the outcome and exit with the Plant job code. */
export async function runDoor(spec: DoorSpec): Promise<never> {
  const { code, detail } = await door(spec);
  console.log(detail);
  process.exit(code);
}
