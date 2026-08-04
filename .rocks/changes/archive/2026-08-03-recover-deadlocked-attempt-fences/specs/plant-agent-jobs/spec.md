## MODIFIED Requirements

### Requirement: Scheduled attempt receipt reconciliation

Plant MUST export the published attempt ID to each job script as
`PLANT_ATTEMPT_ID`. Plant MUST read the keyed Agent Run receipt for an
unresolved nonretryable fence when the job ledger holds no matching record.
Plant MUST append one durable final ledger record for a conclusive receipt
before it clears that fence. Plant MUST retain the fence for an absent,
pending, unreadable, or mismatched receipt. A retained fence's block reason
MUST name the operator command that clears it, and MUST distinguish an absent
receipt from a claimed run that never finished. Plant MUST NOT start another
Herdr lifecycle during reconciliation.

#### Scenario: A job script receives its attempt ID

- WHEN Plant executes a scheduled or manual job script
- THEN the script environment holds `PLANT_ATTEMPT_ID` with the published fence ID
- AND an agent-backed script can supply that value as its Agent Run idempotency key

#### Scenario: A succeeded receipt reconciles a stranded fence

- WHEN an unresolved nonretryable fence has no matching ledger record
- AND the Agent Run receipt for that attempt ID records a succeeded outcome
- THEN Plant appends one durable final ledger record with the `success` outcome
- AND Plant clears the fence without another Herdr launch

#### Scenario: A failed receipt reconciles a stranded fence

- WHEN an unresolved nonretryable fence has no matching ledger record
- AND the Agent Run receipt for that attempt ID records a failed outcome
- THEN Plant appends one durable final ledger record with the `failed` outcome
- AND Plant clears the fence without another Herdr launch

#### Scenario: The receipt is absent

- WHEN an unresolved nonretryable fence has no matching ledger record
- AND no Agent Run receipt exists for that attempt ID
- THEN Plant retains the fence
- AND Plant does not redispatch the job
- AND the block reason names the command that clears the fence

#### Scenario: The receipt is pending

- WHEN the Agent Run receipt for the fence attempt ID remains in progress
- THEN Plant retains the fence
- AND Plant does not redispatch the job
- AND the block reason distinguishes a claimed run from an absent receipt
- AND the block reason names the command that clears the fence

#### Scenario: The receipt is unreadable

- WHEN the Agent Run receipt for the fence attempt ID is corrupt
- THEN Plant retains the fence
- AND Plant reports the unreadable receipt as the block reason
- AND the block reason names the command that clears the fence

## ADDED Requirements

### Requirement: Operator recovery of an abandoned attempt fence

Plant MUST provide an operator command that clears a nonretryable attempt fence
no reconciliation can resolve. The command MUST acquire the same attempt lock a
dispatch takes, so it cannot race a live tick. The command MUST refuse to force
a fence that reconciliation would already clear, reporting instead that the next
tick resolves it, and MUST append no ledger record in that case. When it does
clear a fence the command MUST first append one durable ledger record with the
`failed` outcome naming the abandoned attempt ID, so the abandonment reaches the
job-health sweep rather than passing silently. Running the command against a job
with no fence MUST succeed and change nothing.

#### Scenario: An abandoned attempt is cleared by the operator

- WHEN a nonretryable fence cannot be resolved by reconciliation
- AND the operator runs the unblock command for that job
- THEN Plant appends one durable ledger record with the `failed` outcome naming that attempt ID
- AND Plant clears the fence
- AND a later scheduler tick dispatches the job

#### Scenario: A self-resolving fence is not forced

- WHEN a fence would be cleared by ordinary reconciliation
- AND the operator runs the unblock command for that job
- THEN Plant leaves the fence to reconciliation
- AND Plant reports that the next scheduler tick resolves it
- AND Plant appends no ledger record

#### Scenario: Unblocking a job with no fence is harmless

- WHEN a job holds no attempt fence
- AND the operator runs the unblock command for that job
- THEN the command succeeds
- AND Plant appends no ledger record
