## ADDED Requirements

### Requirement: Preparation-ordered Envelope persistence

Plant MUST atomically reserve a private per-session preparation sequence with
request delta-base advancement and MUST persist Envelopes in that preparation
order. A completed later response MUST be staged outside the Git-backed Session
Capture until every earlier sequence can be persisted. Plant MUST NOT expose the
private sequence in the Envelope schema or seal a generation with an open
reservation or completed stage.

#### Scenario: Later response completes first

- WHEN two requests for one session are prepared in sequence and the later response completes first
- THEN the later completed Envelope is durably staged
- AND both Envelopes are eventually persisted and reconstructed in preparation order

#### Scenario: Ordering gap remains live

- WHEN a completed Envelope is staged behind an earlier live response
- THEN capture acceptance succeeds without holding the completed response only in memory
- AND the raw generation remains ineligible for Sealing until the gap drains

#### Scenario: Different sessions capture concurrently

- WHEN requests for different sessions overlap
- THEN each session enforces its own preparation order without a cross-session persistence lock

### Requirement: Capture persistence recovery

Plant MUST recover every ordering journal and completed stage before accepting
proxy traffic or permitting Sealing, and only after retaining both harness
listeners. Recovery MUST inventory only the canonical current root while
retaining each discovered journal, Session Capture, and stage path. It MUST
require valid journals and mandatory stage root, sequence, request, and
Envelope identity; MUST reconcile retired stages only against exact committed
evidence; MUST surface cleanup failures; MUST preserve every real request
delta; and MUST NOT invent response output. Conflicting or malformed evidence
MUST fail startup without altering persisted capture bytes. Durability under
this requirement covers Plant process crashes, not sudden host power loss.

#### Scenario: Plant restarts with abandoned preparations

- WHEN Plant restarts with reservations whose response streams can no longer finish
- THEN every reservation without an exact completed stage is persisted in sequence as an incomplete Envelope
- AND each incomplete Envelope has `response.complete=false` with no synthesized response body
- AND matching completed stages are interleaved at their reserved positions before new reservations are accepted

#### Scenario: Append completed before stage cleanup

- WHEN recovery finds an undeleted stage and the final complete persisted Envelope has the same `request_id` and exactly matches it
- THEN recovery treats the stage as committed and removes the leftover stage without duplicating the Envelope

#### Scenario: Drain crashed during the final append

- WHEN the final live bytes are an exact prefix of the retained staged Envelope
- THEN recovery may remove only that incomplete tail and append the full staged Envelope
- AND any nonmatching or conflicting tail remains untouched and causes startup to fail

#### Scenario: Incomplete Envelope append is retried

- WHEN recovery retries an abandoned reservation whose identical incomplete Envelope is already complete or present as an exact partial prefix
- THEN recovery leaves or replaces the tail so exactly one complete incomplete Envelope remains
- AND conflicting persisted content remains unchanged and fails startup

#### Scenario: Legacy state has no ordering fields

- WHEN a legacy `state.json` has a valid request delta base and no private stage backlog
- THEN Plant preserves that delta base and initializes an empty ordering journal lazily

#### Scenario: Stage exists without a matching journal

- WHEN private stages exist without a readable journal for the same canonical Session Capture root and session
- THEN Plant leaves persisted evidence unchanged and fails startup with actionable session and sequence diagnostics

#### Scenario: Persisted evidence moved within the current root

- WHEN recovery discovers a journal or stage at a Session Capture path different from mutable metadata or a cached date
- THEN it reconciles that exact discovered path without creating a new dated directory

#### Scenario: Persisted recovery evidence is invalid

- WHEN a journal is missing, corrupt, or wrongly shaped, or a stage omits or conflicts on root, sequence, request, or Envelope identity
- THEN Plant rejects the evidence without treating it as an empty legacy state

#### Scenario: Retired stage remains after journal commit

- WHEN a stage below `next_to_drain` exactly matches the final committed Envelope
- THEN recovery removes the retired stage without appending it again
- AND any mismatch fails while preserving the stage and capture bytes

#### Scenario: Stage cleanup fails

- WHEN a reconciled stage cannot be removed
- THEN recovery reports the cleanup failure and preserves the stage for an idempotent retry

### Requirement: Immutable generation Sealing

Plant MUST recheck Sealing eligibility under the per-session capture mutex and
atomically detach the eligible raw generation so subsequent captures write a
fresh raw file. The detached generation MUST remain reconstructable and
digest-identified until its zstd frame is committed exactly once. A retry MUST
distinguish an uncommitted destination from the exact post-rename,
pre-detached-removal state. Existing Envelope and concatenated-zstd generation
formats MUST remain readable.

#### Scenario: Capture finishes at the Sealing boundary

- WHEN response completion and generation detachment contend for the same session mutex
- THEN the completed Envelope is either included in the detached generation or keeps that generation ineligible
- AND no Envelope is lost

#### Scenario: Plant restarts with a detached generation

- WHEN Plant restarts after detachment, destination rename, or detached-file removal
- THEN Sealing resumes or recognizes the committed generation without omission or duplication

#### Scenario: Reconstruction overlaps Sealing

- WHEN sealed, detached, and newer live raw generations coexist
- THEN Reconstruction reads every generation exactly once in chronological order
- AND errors identify only locations rather than captured content

### Requirement: Complete daemon ownership before recovery

Plant MUST bind and retain both configured harness listeners before capture
recovery or scheduler startup. On any partial collision it MUST release acquired
listeners and exit zero only when both health endpoints identify the expected
complete incumbent Plant; otherwise it MUST fail nonzero.

#### Scenario: Complete incumbent already owns both harnesses

- WHEN another Plant identifies itself correctly on both configured health endpoints
- THEN the losing process exits zero without running recovery

#### Scenario: Only one port is occupied

- WHEN one harness listener is occupied without a complete matching incumbent
- THEN Plant releases any listener it acquired, exits nonzero, and performs no recovery mutation

### Requirement: Complete Envelope record reconstruction

Reconstruction MUST recover every complete Envelope JSON value from persisted
terminated records, including multiple concatenated values in one legacy record.
It MUST ignore whitespace-only records and MUST return a location-only error for
terminated non-whitespace residue that cannot form complete Envelopes. Only an
unterminated final fragment in a live raw Session Capture MAY be ignored; a
sealed Session Capture MUST fail on incomplete or malformed trailing content.

#### Scenario: Legacy record contains concatenated Envelopes

- WHEN one terminated record contains two or more complete concatenated Envelope JSON values followed by optional whitespace
- THEN Reconstruction applies every Envelope in its persisted order
- AND the whitespace contributes no Envelope

#### Scenario: Terminated record contains unrecoverable residue

- WHEN a terminated sealed or raw record contains non-whitespace residue that cannot form complete Envelopes
- THEN Reconstruction fails with the segment and one-based record number
- AND the error does not echo captured content

#### Scenario: Live raw file ends during an append

- WHEN a live raw Session Capture ends with one unterminated JSON fragment
- THEN Reconstruction succeeds through the last complete Envelope and ignores only that final fragment

#### Scenario: Sealed file has an incomplete tail

- WHEN a sealed Session Capture contains incomplete or malformed trailing JSON
- THEN Reconstruction fails instead of silently returning a partial history
