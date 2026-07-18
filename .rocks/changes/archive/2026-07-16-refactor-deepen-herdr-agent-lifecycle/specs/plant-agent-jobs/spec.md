## ADDED Requirements

### Requirement: Deep Herdr agent lifecycle

Plant MUST run each agent-backed Cultivation Job through one high-level Herdr lifecycle interface that owns workspace creation and reclamation, verified agent readiness, prompt delivery, completion waiting, and best-effort cleanup with focus restoration. The scheduler MUST retain job selection, launch construction, prompts, cadence, and outcome recording.

#### Scenario: Herdr is unavailable before an attempt

- WHEN the initial Herdr availability check fails
- THEN the lifecycle returns `Unavailable`
- AND Plant does not record an attempt so a later scheduler tick may retry

#### Scenario: Startup or prompt delivery fails

- WHEN Herdr is available but workspace creation, CLI startup, or prompt delivery fails
- THEN the lifecycle returns `Failed` with a diagnostic detail
- AND Plant records the failed attempt using the existing cadence policy

#### Scenario: Agent run succeeds

- WHEN a verified agent pane receives the prompt and reaches completion
- THEN the lifecycle returns `Succeeded` with a diagnostic detail
- AND applies the supplied `Never`, `Always`, or `OnSuccess` cleanup policy without changing user focus

#### Scenario: Cleanup fails after success

- WHEN the agent succeeds but stale-workspace reclamation or final cleanup fails
- THEN the lifecycle remains `Succeeded`
- AND cleanup failure does not expand the outcome contract or change scheduler recording
