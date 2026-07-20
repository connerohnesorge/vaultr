# Vault Doors — Delta

## ADDED Requirements

### Requirement: Door library owns the door routine

A shared TypeScript library in this workspace MUST provide a `door` entry
point taking a watch glob, an optional filter predicate, and a prompt builder,
and MUST own new-file detection, dedup fencing, watch-root policy, fire-rate
breaking, and agent launch — so an individual door job contains only its
watch, filter, and prompt.

#### Scenario: A door is a ten-line job

- WHEN a door job imports the library and calls `door` with watch, filter, and prompt
- THEN detection, fencing, policy, breaking, and launch behavior come from the library
- AND the door script contains no hand-rolled fencing or launch code

### Requirement: Crash-idempotent ordered batch claims

A door MUST fail closed on unreadable or invalid state and MUST serialize each
door's evaluation with a per-door cross-process lock. It MUST persist a
timestamp frontier and the durable sorted set of paths already processed at
that timestamp, select all unseen files in total `(mtime,path)` batch order,
atomically persist the exact ordered in-progress batch before launch, and
derive a stable Plant Agent Run idempotency key from that claim. A retry MUST
resume the persisted claim. Only a machine-readable Plant result confirming a
durably recorded `Succeeded` or `Failed` outcome MAY atomically advance the
frontier through the claim and clear it. Retryable, operationally failed,
missing, malformed, or otherwise indeterminate results MUST retain the claim
and key. A given file MUST never launch a second agent run.

#### Scenario: A sync batch produces one session

- WHEN a sync lands 20 matching files before the door's next run
- THEN the door launches exactly one agent session whose prompt references all 20
- AND the timestamp frontier records them after the outcome is durably recorded

#### Scenario: Nothing new means no launch

- WHEN no matching file is newer than the timestamp frontier or unseen at its timestamp
- THEN the door exits without contacting Herdr

#### Scenario: Corrupt state fails closed

- WHEN a door's state cannot be parsed or violates the state schema
- THEN the door returns a failure without replacing the state
- AND no agent session is launched

#### Scenario: Concurrent door processes

- WHEN two processes evaluate the same door concurrently
- THEN the per-door lock permits only one process to claim and launch the batch
- AND the other process does not launch it

#### Scenario: Files share a timestamp

- WHEN `z.md` is durably processed and `a.md` lands later with the same mtime
- THEN the durable seen-path set includes `z.md` without treating the timestamp as closed
- AND `a.md` is included on the next evaluation without re-firing `z.md`

#### Scenario: Door crashes before launch

- WHEN a door crashes after persisting its claim but before calling Plant
- THEN the next invocation resumes the same ordered claim and idempotency key
- AND the batch launches once

#### Scenario: Door crashes after launch

- WHEN a door crashes after Plant durably records the launch outcome but before the frontier save
- THEN the next invocation reuses the persisted key and Plant's durable prior outcome
- AND no second Herdr workspace is created

### Requirement: Ingestion-only watch roots

The library MUST select exactly one allowlisted ingestion root — a path written
only by sync jobs — for each watch glob. It MUST reject traversal and MUST
canonicalize the selected root and every scanned or hydrated file, rejecting
any real path outside that selected root, including symlink escapes, before
reading content or launching an agent. A door whose watch glob falls outside
the allowlist MUST fail loudly before launch, so a door cannot subscribe to
agent-written Vault Content.

#### Scenario: Watching agent-written content is rejected

- WHEN a door's watch glob targets a cultivation path such as learnings
- THEN the library refuses to evaluate the door and records the rejection
- AND no agent session is launched

#### Scenario: Traversal and symlink escapes fail closed

- WHEN a watch contains traversal or a matching or claimed file resolves through a symlink outside the selected ingestion root
- THEN the library returns a failure before reading that file or launching an agent
- AND any durable in-progress claim remains intact

### Requirement: Rolling-window fire breaker

The library MUST pause a door that exceeds the configured fires-per-window
limit, record the pause loudly in the door's ledger, and require a deliberate
manual re-arm before the door fires again.

#### Scenario: A runaway door is paused

- WHEN a door exceeds the fire limit within the rolling window
- THEN its next evaluation is skipped and the pause is recorded
- AND the door stays paused until manually re-armed

### Requirement: Typed launch over plant agent run

The library MUST launch agent sessions only through `plant agent run`, MUST
pass the persisted claim's stable idempotency key, and MUST require Plant's
machine-readable result separating durable `Succeeded`/`Failed` outcomes from
retryable and indeterminate state. It MUST NOT reimplement any part of the
Herdr lifecycle owned by Plant.

#### Scenario: Non-durable result does not advance the claim

- WHEN `plant agent run` reports a retryable or indeterminate result or does not emit a valid result
- THEN the door's timestamp frontier does not advance
- AND the in-progress claim and idempotency key remain durable
- AND the same files are eligible on the next run
