# Capture Stewardship Specification

## MODIFIED Requirements

### Requirement: Capture persistence recovery

Plant MUST recover every ordering journal and completed stage before accepting
proxy traffic or permitting Sealing, and only after retaining both harness
listeners. Recovery MUST inventory only the canonical current root while
retaining each discovered journal, Session Capture, and stage path. It MUST
reject symlinked numeric date or session levels and any canonical session path
outside the canonical root. A private strict Journal loader MUST explicitly
validate legacy state and, when `capture_order` is present, require every
ordering field and valid bound and request identity. Recovery MUST retain the
parsed Journal through application, require mandatory stage root, sequence,
request, and Envelope identity, reconcile retired stages against exact
committed evidence, surface journal and cleanup failures, preserve every real
request delta, and MUST NOT invent response output. Conflicting or malformed
evidence MUST fail startup without altering persisted capture bytes. Tail
reconciliation MUST scan backward independently of the staged record size and
classify blank, valid terminated, malformed terminated, and unterminated
evidence. It MUST append after a valid different request, accept a same-request
record only when byte-exact, and repair only an exact staged prefix. The
classifier MUST skip trailing whitespace-only records and fragments, classify
the file as blank only when every byte is whitespace, and use the same
bounded-memory concatenated-value decoder as Reconstruction to isolate the final
complete Envelope without materializing the complete record or a generic JSON
tree. Every accepted value MUST contain a valid UUID `request_id`, and any
terminated residue MUST be rejected without mutation. Recovery MUST open the
raw generation through its retained session-directory descriptor without
following symlinks and MUST use that same raw descriptor for classification,
comparison, append, and repair. Durability under this requirement covers Plant
process crashes, not sudden host power loss.

Stage removal MUST be idempotent. An already absent stage file MUST count as a
successful removal, because an absent file is the intended end state. Every
other removal error MUST still fail.

Retired stage reconciliation MUST accept any stage below the drain head whose
Envelope is already present in the raw generation. Reconciliation MUST NOT
require the stage to sit immediately below the drain head.

#### Scenario: Plant restarts with abandoned preparations

- WHEN Plant restarts with reservations whose response streams can no longer finish
- THEN every reservation without an exact completed stage is persisted in sequence as an incomplete Envelope
- AND each incomplete Envelope has `response.complete=false` with no synthesized response body
- AND matching completed stages are interleaved at their reserved positions before new reservations are accepted

#### Scenario: Append completed before stage cleanup

- WHEN recovery finds an undeleted stage and the final complete persisted Envelope has the same `request_id` and exactly matches it
- THEN recovery treats the stage as committed and removes the leftover stage without duplicating the Envelope

#### Scenario: Stage file is already absent at removal

- WHEN recovery removes a stage file that another pass already removed
- THEN recovery treats the removal as successful and continues the drain

#### Scenario: Orphan stages sit far below the drain head

- WHEN recovery finds retired stages many sequences below the drain head and each Envelope is already present in the raw generation
- THEN recovery removes every one of those stages and continues the drain

## ADDED Requirements

### Requirement: Fault-isolated drain sweep

The periodic drain sweep MUST isolate every Session Capture failure. A failure
on one Session Capture MUST NOT stop the sweep from processing every remaining
Session Capture. The sweep MUST record each per-session failure and MUST report
an aggregate failure after it processes the complete inventory. A failure to
build the inventory MUST still fail the sweep immediately, because no Session
Capture is safe to process without a complete inventory. Startup recovery MUST
keep failing on the first error, because Plant MUST NOT serve traffic over
unverified capture evidence.

#### Scenario: One damaged Session Capture does not starve the others

- WHEN a periodic sweep fails on one Session Capture and other Session Captures have a stranded backlog
- THEN the sweep drains every other Session Capture in the same pass
- AND the sweep reports an aggregate failure naming each failed Session Capture

#### Scenario: Inventory failure stops the sweep

- WHEN a periodic sweep cannot build a complete Session Capture inventory
- THEN the sweep fails without applying any Session Capture

#### Scenario: Startup recovery stays fail-closed

- WHEN startup recovery fails on one Session Capture
- THEN Plant fails to start and accepts no proxy traffic

### Requirement: Irreconcilable stage quarantine

Plant MUST quarantine a stage that recovery cannot reconcile against the raw
generation, so that stage stops blocking the drain permanently. Quarantine MUST
move the stage file under a quarantine directory beside the staging root without
altering its bytes. Plant MUST NOT delete irreconcilable evidence. Plant MUST
record one dropped turn for each quarantined stage through the existing drop
accounting, including the reason and the sequence. After quarantine, recovery
MUST continue draining the remaining sequences for that Session Capture.
Quarantine MUST apply to the periodic sweep only. Startup recovery MUST keep
failing on irreconcilable evidence.

#### Scenario: Conflicting stage is moved aside

- WHEN a periodic sweep finds a stage whose Envelope conflicts with the raw generation
- THEN the sweep moves that stage into the quarantine directory with its bytes unchanged
- AND the sweep records one dropped turn naming the reason and the sequence
- AND the sweep drains the remaining sequences for that Session Capture

#### Scenario: Quarantine never deletes evidence

- WHEN a stage is quarantined
- THEN the staged bytes remain readable under the quarantine directory

#### Scenario: Startup keeps failing on conflicting evidence

- WHEN startup recovery finds a stage whose Envelope conflicts with the raw generation
- THEN Plant fails to start and quarantines nothing

### Requirement: Stranded backlog accounting

Plant MUST account for a staged Envelope that never drains, so an incomplete
Session Capture is never silent. Plant MUST count the staged Envelopes that
remain undrained for each Session Capture. Plant MUST report the total stranded
backlog on the health endpoint beside the unrecorded drop count. When Plant
seals a Session Capture generation with a stranded backlog, Plant MUST record
one dropped turn for each undrained Envelope.

#### Scenario: Health reports a stranded backlog

- WHEN a Session Capture holds staged Envelopes that no sweep has drained
- THEN the health endpoint reports the total stranded backlog count

#### Scenario: Sealing records an undrained backlog

- WHEN Plant seals a generation for a Session Capture with undrained staged Envelopes
- THEN Plant records one dropped turn for each undrained Envelope

#### Scenario: Drained Session Capture reports no backlog

- WHEN every staged Envelope for a Session Capture has drained
- THEN the health endpoint reports a stranded backlog of zero
