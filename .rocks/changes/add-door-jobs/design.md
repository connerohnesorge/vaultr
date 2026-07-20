# Design — Add Door Jobs

## Context

Doors were grilled to a settled shape before this proposal: triggers come only
from vault file changes (sync jobs are the ingestion layer), detection is
poll-on-cadence rather than filesystem watching, and the door logic lives in
typed code rather than bash or a bespoke declarative format.

## Decision summary

- A door is a job. No door engine, no `vault/doors/*.toml`, no second
  scheduler: Plant's existing filename-cadence scanner runs door scripts like
  any other Cultivation Job, and outcomes land in the existing jobs ledger.
- The fragile parts — dedup fencing, watch-root policy, fire-rate breaking,
  agent launch — are written once in a shared TypeScript library, not
  re-authored per door.
- Door state fails closed, is replaced atomically under a per-door
  cross-process lock, and carries an ordered `(mtime,path)` cursor plus the
  exact in-progress batch. The claim deterministically supplies Plant's
  idempotency key, so process crashes cannot create a second agent run.
- Loop prevention is structural first (doors may only watch ingestion roots
  that agents never write) with a rate breaker as defense-in-depth, because a
  self-sustaining door fires slowly (agent runtime spaces the fires) and would
  slide under naive consecutive-fire detection.

## ADRs

### ADR-0001: Jobs exec via shebang, not a hardwired interpreter

Plant currently spawns `/bin/bash <script>` for every job. It will instead
exec the job path directly and let the shebang choose the interpreter, with
the filename parse extension-agnostic (`<name>.<interval>.<ext>`). This is the
whole polyglot mechanism: Plant learns nothing about Bun, Python, or any
future language. Consequence: every job must be executable; measured on macOS,
a shebang-less script does NOT fail — the exec fallback runs it via `/bin/sh` —
so legacy jobs keep working, and the shebang sweep + `vault/jobs/AGENTS.md`
contract update are for explicitness, not survival.
Rejected alternative: mapping extensions to interpreters inside Plant —
recurring special-casing for no added capability.

### ADR-0002: Doors are jobs, not a separate engine

Rejected alternatives: per-door bash scripts (each reimplements fencing —
the exact code that must not be flaky, since a double-fire means duplicate
agent sessions) and a resident door engine reading declarative TOML (a second
scheduler and a filter DSL that TypeScript predicates make unnecessary).
Doors-as-jobs deletes both: cadence comes from the filename, scheduling and
outcome recording come from Plant, and the door file is real code where real
code is wanted (filters, prompt building).

### ADR-0003: TS library wraps idempotent `plant agent run`; lifecycle stays in Plant

The jobs contract already requires agent jobs to drive panes only via
`plant agent run`, and `plant-agent-jobs` made Plant's Herdr lifecycle the
single owner of workspace creation, readiness, delivery, wait, and cleanup.
The library is a thin typed client over that interface: it supplies the
persisted batch's stable idempotency key and receives structured
`Unavailable`/`Failed`/`Succeeded` results, never a second lifecycle
implementation. Plant durably claims each key before Herdr side effects,
records conclusive outcomes before returning, and returns an already-recorded
outcome without creating another workspace. An unavailable pre-launch Herdr
probe remains retryable and does not become a conclusive outcome.

### ADR-0004: Loop prevention is allowlist-by-construction plus a rate breaker

Door-launched agents write Vault Content; a door watching agent-written paths
(e.g. learnings) would self-sustain, and write-attribution tagging is not
reliably implementable from outside the agent. Instead the library enforces an
allowlist of watchable ingestion roots (paths written only by sync jobs) and
rejects any other watch glob loudly on first run. A rolling-window breaker
(more than N fires per hour pauses the door, logs to its ledger, and requires
manual re-arm) backstops allowlist mistakes. Rejected: consecutive-fire
breakers (real loops fire slowly and never trip them) and provenance tagging
(machinery disproportionate to the residual risk once paths are partitioned).

### ADR-0005: Door batches are ordered durable claims

A scalar mtime cannot distinguish files with tied timestamps, and saving it
after launch leaves a crash window that can launch the same batch twice. Each
door therefore serializes evaluation with a per-door cross-process lock, fails
closed on unreadable or invalid state, and atomically persists the exact batch
ordered by `(mtime,path)` before launch. The persisted claim determines the
Plant idempotency key and survives both pre-launch and post-launch Door
crashes. A retry resumes that claim; after a conclusive Plant outcome it
advances the cursor and clears the claim atomically. `Unavailable` retains the
claim and its key for a later attempt.
