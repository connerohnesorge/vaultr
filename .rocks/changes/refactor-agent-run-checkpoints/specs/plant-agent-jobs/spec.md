## MODIFIED Requirements

### Requirement: Recoverable keyed Agent Run identity

Plant MUST persist a tagged phase-specific checkpoint in each keyed Agent Run
receipt. The checkpoint MUST preserve the immutable Herdr workspace and pane
identity. The checkpoint MUST represent terminal-only and session-bound pane
identity as distinct states. A captured checkpoint MUST include the captured
session. Plant MUST reconcile a pending receipt against the exact checkpoint
identity. Plant MUST NOT use receipt age as execution evidence. Plant MUST NOT
create another workspace while the recorded execution can still finish. Plant
MUST retain the fence when Herdr or identity evidence is unavailable. Legacy
pending receipts MUST require verified operator recovery. The Codex Learn
wrapper and 15-minute cadence MUST remain unchanged.

#### Scenario: The recorded pane remains working

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- AND the exact recorded pane remains working
- WHEN Plant reconciles the receipt
- THEN Plant resumes observation of that pane
- AND Plant does not create another Herdr workspace

#### Scenario: The recorded session completed

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- AND a captured checkpoint proves submitted work
- AND the matching captured session contains a terminal response
- WHEN Plant reconciles the receipt
- THEN Plant persists one conclusive successful receipt
- AND Plant appends one successful job ledger record
- AND Plant clears the attempt fence

#### Scenario: The recorded execution cannot finish

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- AND Herdr proves the recorded execution is absent
- AND the matching captured session has no terminal response
- WHEN Plant reconciles the receipt
- THEN Plant persists one conclusive failed receipt
- AND Plant appends one failed job ledger record
- AND Plant clears the attempt fence

#### Scenario: Recovery evidence is unavailable

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- WHEN Herdr is unavailable or the recorded identity conflicts
- THEN Plant retains the pending receipt
- AND Plant retains the attempt fence
- AND Plant does not create another Herdr workspace

#### Scenario: A legacy pending receipt lacks identity

- GIVEN a pending keyed Agent Run receipt stores only its attempt key
- WHEN Plant reconciles the receipt
- THEN Plant retains the attempt fence
- AND Plant names `plant jobs unblock <name>` as the operator recovery

#### Scenario: A terminal-only checkpoint awaits session discovery

- GIVEN a pending keyed Agent Run receipt stores a terminal-only checkpoint
- WHEN the captured session identifier is not available
- THEN Plant retains the pending receipt
- AND Plant does not infer a capture session

#### Scenario: Checkpoint identity is stable

- GIVEN a pending keyed Agent Run receipt stores a checkpoint
- WHEN a later progress update supplies a different workspace, pane, terminal, or session identity
- THEN Plant rejects the update
- AND Plant retains the existing checkpoint

#### Scenario: The current Codex Learn attempt is recovered

- GIVEN session `019fb277-d08d-7f62-a1dd-2115d251056e` contains a terminal response
- AND no live execution owns attempt `ddd1fb63-2eb6-4c17-8bb3-a882f1c497ef`
- WHEN the operator runs `plant jobs unblock learn-codex`
- THEN Plant records the abandoned attempt as failed
- AND a later Codex Learn run can write a durable final record

#### Scenario: Codex Learn scheduling remains unchanged

- GIVEN interrupted Agent Run recovery is deployed
- WHEN Plant scans `vault/jobs/learn-codex.15m.sh`
- THEN Plant schedules Codex Learn every 15 minutes
