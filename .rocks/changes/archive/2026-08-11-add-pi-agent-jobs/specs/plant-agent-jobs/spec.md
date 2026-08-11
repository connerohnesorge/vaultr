## MODIFIED Requirements

### Requirement: Deep Herdr agent lifecycle

Plant MUST run each agent-backed Cultivation Job through one high-level Herdr lifecycle interface that owns workspace creation and reclamation, verified agent readiness, prompt delivery, completion waiting, and best-effort cleanup with focus restoration. Plant MUST reconcile readiness snapshots until the selected supported native-agent pane remains prompt-ready after the composer settles. Plant MUST snapshot that pane's terminal and available agent-session identity before checked `pane run` prompt typing. Plant MUST receive a subscription acknowledgment before prompt typing. If the pane has not entered `working`, Plant MUST send exactly one checked Enter. Plant MUST reconcile buffered lifecycle events with same-pane snapshots. Plant MUST observe post-submit `working` before accepting a later terminal state. Plant MUST recheck terminal/session identity before returning success. Failed typing, failed submission, a missing transition, pre-existing terminal state, or a pane identity change MUST NOT return `Succeeded`. The scheduler MUST retain job selection, launch construction, prompts, cadence, and outcome recording.

#### Scenario: Herdr is unavailable before an attempt

- WHEN the initial Herdr availability check fails
- THEN the lifecycle returns `Unavailable`
- AND Plant does not record an attempt so a later scheduler tick may retry

#### Scenario: Startup or prompt delivery fails

- WHEN Herdr is available but workspace creation, CLI startup, subscription acknowledgment, prompt typing, or required Enter delivery fails
- THEN the lifecycle returns `Failed` with a diagnostic detail
- AND Plant records the failed attempt using the existing cadence policy

#### Scenario: Agent run succeeds

- WHEN a verified supported native-agent pane is idle or done
- AND the lifecycle acknowledges its pane-scoped status subscription before checked `pane run` prompt typing
- AND the lifecycle sends one checked Enter only if the pane is not already `working`
- AND Plant observes `working` followed by a terminal state through its buffered stream or same-pane snapshots
- AND the terminal and available agent-session identities remain unchanged
- THEN the lifecycle returns `Succeeded` with a diagnostic detail
- AND applies the supplied `Never`, `Always`, or `OnSuccess` cleanup policy without changing user focus

#### Scenario: Pre-existing done is not this run

- WHEN the selected pane is already `done` before prompt submission but no post-acknowledgment `working` transition is observed
- THEN the lifecycle returns `Failed`
- AND Plant MUST NOT durably report the run as succeeded

#### Scenario: Readiness changes while the composer settles

- WHEN a selected supported native-agent pane becomes non-ready after initial readiness
- THEN Plant continues waiting for the same pane identity
- AND Plant does not type the prompt before readiness returns

#### Scenario: A terminal event is absent

- WHEN Plant observes post-submit `working`
- AND a same-pane snapshot later reports `idle` or `done`
- AND the terminal event is absent from the subscription
- THEN Plant returns success after identity verification

#### Scenario: Pane identity changes during the turn

- WHEN the pane's terminal or captured agent-session identity changes after submission
- THEN the lifecycle returns `Failed` even if a terminal status is observed
- AND Plant MUST NOT durably report the run as succeeded

#### Scenario: Agentless or unknown pane is observed

- WHEN the selected pane is agentless, unknown, or not a supported native-agent pane in idle or done state
- THEN Plant MUST NOT deliver the prompt
- AND the lifecycle fails with diagnostic detail while applying its cleanup policy

#### Scenario: Cleanup fails after success

- WHEN the agent succeeds but stale-workspace reclamation or final cleanup fails
- THEN the lifecycle remains `Succeeded`
- AND cleanup failure does not expand the outcome contract or change scheduler recording

## ADDED Requirements

### Requirement: Pi agent job launches

`plant agent run` MUST accept `--cli pi`. Usage and invalid-value errors MUST list `pi` as a supported launch identity. Plant MUST map this identity to Herdr's `pi` agent. Pi panes MUST participate in all supported native-agent pane gates and recovery checks. Plant MUST render Pi launches with `PLANT_AGENT=1`, project trust approval, the `openai-codex` provider, Pi's `--model` option, and Pi's `--thinking` option. Plant MUST give every Pi run a unique `--session-dir` under Plant state. Plant MUST read the first JSONL record ID from that directory and register it as the job self-capture before workspace cleanup.

#### Scenario: Pi is selected

- WHEN an operator supplies `plant agent run --cli pi`
- THEN Plant accepts the command
- AND usage and invalid-value errors list `pi`
- AND Plant expects Herdr to report the pane agent as `pi`

#### Scenario: Pi launch options are rendered

- WHEN Plant launches Pi with a model and effort
- THEN the launch starts with `PLANT_AGENT=1 command pi --approve --provider openai-codex`
- AND Plant renders the model with `--model`
- AND Plant renders the effort with `--thinking`

#### Scenario: Pi run session is isolated

- WHEN Plant prepares a Pi run
- THEN Plant appends a unique `--session-dir` under Plant state
- AND no other prepared Pi run receives the same directory

#### Scenario: Pi self-capture is registered

- WHEN the sole Pi JSONL session file starts with `{"type":"session","id":"<uuid>"}`
- AND the Pi run reaches a terminal state
- THEN Plant registers that record ID as the job self-capture before cleanup

#### Scenario: Native effort flags are smuggled through extra arguments

- WHEN Pi extra arguments contain `--thinking`
- THEN Plant rejects the command
- AND the operator must use `--effort`

#### Scenario: Existing launch identities remain supported

- WHEN Plant launches Claude, Codex, or Prime
- THEN Plant preserves the existing launch and self-capture behavior
