# Capture Stewardship — Delta

## ADDED Requirements

### Requirement: Resumed sessions reopen sealed captures

Plant MUST, before appending a capture envelope to a session dir where
`turns.jsonl.zst` exists without a raw `turns.jsonl`, unseal the capture back
to raw (removing the seal) so the session continues as a single epoch; the
`herdr.jsonl` sidecar MUST get the same treatment. Unseal failure MUST NOT
fail or delay the envelope write — Plant falls back to the fresh-epoch append
and leaves reconciliation to the seal-reconciliation pass.

#### Scenario: Session resumes after its capture sealed

- WHEN an envelope arrives for a session dir containing only `turns.jsonl.zst`
- THEN the seal is unsealed to `turns.jsonl` and the envelope is appended to it
- AND the dir holds a single raw capture containing pre-resume and post-resume turns

#### Scenario: Unseal fails at capture time

- WHEN the unseal attempt fails
- THEN the envelope is still appended to a fresh raw `turns.jsonl`
- AND the dir is left in the double-file state for seal reconciliation

### Requirement: Seal reconciliation of divergent captures

Plant MUST reconcile every session dir containing both a raw capture file and
its seal before attempting to seal it: byte-identical content removes the raw
duplicate; when one side is a line-prefix of the other the superset becomes
the raw capture; otherwise the merged raw capture is seal-content followed by
raw-content. Merges MUST be written atomically, MUST be verified to cover both
parts before the stale seal is removed, and MUST apply equally to the
`herdr.jsonl` sidecar. On verification failure both files MUST be left in
place for the next tick.

#### Scenario: Raw file duplicates the seal

- WHEN a dir holds a raw capture byte-identical to its seal's content
- THEN the raw duplicate is removed and the seal stands

#### Scenario: Post-resume fresh epoch

- WHEN a dir holds a raw capture that shares no prefix with its seal's content
- THEN the merged raw capture is the seal's turns followed by the raw turns
- AND the stale seal is removed only after the merge verifies

#### Scenario: Raw capture supersedes the seal

- WHEN the seal's content is a line-prefix of the raw capture
- THEN the raw capture is kept unchanged and the stale seal is removed

#### Scenario: Empty or corrupt seal

- WHEN a dir holds a seal that decompresses to zero lines alongside a raw capture
- THEN the raw capture is kept and the empty seal is removed

#### Scenario: Merge verification fails

- WHEN the merged file does not cover both parts
- THEN neither the raw capture nor the seal is deleted
- AND the next reconciliation tick retries

## MODIFIED Requirements

### Requirement: Stuck Session Capture detection

Plant MUST classify every raw (unsealed) Session Capture idle for at least a
configurable age (default 24 hours): `divergent` when the capture's seal also
exists in the dir, and otherwise by its learn-ledger state — `seal-blocked`
when every learner has ledgered it yet it remains unsealed, `half-learned`
naming the missing learner when only a strict subset of learners ledgered it,
`unlearned` when no learner ledgered it and it passes the learn substance
gate, and `sub-threshold` when no learner ledgered it and it falls below the
substance gate. Detection MUST be read-only against Session Captures.

#### Scenario: Capture diverged from its seal

- WHEN a raw Session Capture idle beyond the age threshold coexists with its seal
- THEN it is reported as `divergent` regardless of ledger state

#### Scenario: Sealing is failing on a fully learned capture

- WHEN a raw Session Capture idle beyond the age threshold is ledgered by every learner
- THEN it is reported as `seal-blocked`

#### Scenario: One learner never processed a capture

- WHEN a raw Session Capture idle beyond the age threshold is ledgered by only one learner
- THEN it is reported as `half-learned` naming the missing learner

#### Scenario: Learn never picked up a substantive capture

- WHEN a raw Session Capture idle beyond the age threshold has no ledger entry and passes the substance gate
- THEN it is reported as `unlearned`

#### Scenario: Capture below the substance gate

- WHEN a raw Session Capture idle beyond the age threshold has no ledger entry and falls below the substance gate
- THEN it is reported as `sub-threshold`

#### Scenario: Active and sealed captures are exempt

- WHEN a raw Session Capture was modified within the age threshold, or a Session Capture is already sealed
- THEN it is not reported

### Requirement: Watchdog Cultivation Job

Plant MUST run an in-process `watchdog` job every 6 hours that performs stuck
detection and records the outcome: `failed` with per-state counts when any
`divergent`, `seal-blocked`, `half-learned`, or `unlearned` capture exists,
otherwise `success`. Sub-threshold captures MUST be counted in the recorded
detail but MUST NOT fail the job. The job launches no agent pane and writes
nothing to Session Captures.

#### Scenario: Actionable stuck captures exist

- WHEN detection finds at least one divergent, seal-blocked, half-learned, or unlearned capture
- THEN the job logs one line per stuck capture and records `failed` with per-state counts

#### Scenario: Only sub-threshold captures exist

- WHEN detection finds only sub-threshold captures
- THEN the job records `success` with the sub-threshold count in the detail

#### Scenario: Pipeline is healthy

- WHEN detection finds nothing stuck
- THEN the job records `success`
