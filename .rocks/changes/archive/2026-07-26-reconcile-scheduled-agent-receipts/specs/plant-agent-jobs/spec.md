# Plant Agent Jobs Delta

## ADDED Requirements

### Requirement: Scheduled attempt receipt reconciliation

Plant MUST export the published attempt ID to each job script as
`PLANT_ATTEMPT_ID`. Plant MUST read the keyed Agent Run receipt for an
unresolved nonretryable fence when the job ledger holds no matching record.
Plant MUST append one durable final ledger record for a conclusive receipt
before it clears that fence. Plant MUST retain the fence for an absent,
pending, unreadable, or mismatched receipt. Plant MUST NOT start another Herdr
lifecycle during reconciliation.

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

#### Scenario: The receipt is pending

- WHEN the Agent Run receipt for the fence attempt ID remains in progress
- THEN Plant retains the fence
- AND Plant does not redispatch the job

#### Scenario: The receipt is unreadable

- WHEN the Agent Run receipt for the fence attempt ID is corrupt
- THEN Plant retains the fence
- AND Plant reports the unreadable receipt as the block reason
