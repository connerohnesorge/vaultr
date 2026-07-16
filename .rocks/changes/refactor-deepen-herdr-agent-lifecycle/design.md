## Context

`jobs.rs` currently contains job policy and roughly 240 lines of Herdr mechanics: raw CLI commands, JSON paths, stale-workspace reclamation, focus restoration, agent verification, TUI startup timing, prompt-landing retries, completion waits, and cleanup. `herdr.rs` already owns the typed pane schema and socket-based topology snapshots.

The deepening concentrates Herdr knowledge without pretending the socket and CLI are interchangeable adapters. The socket remains the implementation used for topology snapshots; the CLI remains the implementation used for workspace mutations.

## Goals / Non-Goals

- Goal: one high-leverage interface for the complete agent-run lifecycle.
- Goal: keep job policy and Herdr mechanics local to their respective modules.
- Goal: preserve every current outcome, cadence, focus, and cleanup behavior.
- Non-Goal: a generic Herdr trait, command-runner seam, lifecycle framework, or explicit state machine.
- Non-Goal: changes to the interactive Herdr plugin shell actions.
- Non-Goal: changes to agent selection, models, flags, prompts, or scheduling.

## Interface

The existing `plant::herdr` module will expose one operation equivalent to:

```rust
pub struct AgentRun {
    pub label: String,
    pub cwd: String,
    pub launch: String,
    pub prompt: String,
    pub timeout: Duration,
    pub cleanup: WorkspaceCleanup,
}

pub enum WorkspaceCleanup {
    Never,
    Always,
    OnSuccess,
}

pub enum AgentRunOutcome {
    Unavailable,
    Succeeded(String),
    Failed(String),
}

pub async fn run_agent(run: AgentRun) -> AgentRunOutcome;
```

The exact ownership and borrowing may follow existing Rust style; the interface concepts and outcome semantics are fixed.

## Decisions

- `jobs.rs` resolves `PLANT_KEEP_PANES` and job retention policy into `WorkspaceCleanup` before calling Herdr.
- `Unavailable` applies only when the initial Herdr availability check fails and remains unrecorded; every subsequent startup, delivery, timeout, or agent failure returns `Failed` and remains a recorded attempt.
- Stale-workspace reclamation and final cleanup remain best-effort mechanics and do not turn `Succeeded` into `Failed`.
- `jobs.rs` supplies the complete Claude or Codex launch command; Herdr does not know job kinds, models, or agent-specific flags.
- Tests cover pure parsing and decisions; a real Herdr smoke check covers the effectful sequence.

## Risks / Trade-offs

- Lifecycle behavior has changed repeatedly and a missed invariant could type into a shell, duplicate a prompt, steal focus, or close a working workspace. Mitigation: move current behavior before simplifying it, retain regression comments, and run the live smoke check.
- `herdr.rs` grows to own both topology snapshots and agent runs. This is intentional locality around one external tool; split only after concrete unrelated change pressure appears.
