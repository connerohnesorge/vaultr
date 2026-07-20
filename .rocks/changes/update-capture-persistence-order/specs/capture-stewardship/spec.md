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
reject symlinked numeric date or session levels and any canonical session path
outside the canonical root. A private strict Journal loader MUST explicitly
validate legacy state and, when `capture_order` is present, require every
ordering field and valid bound and request identity. Recovery MUST retain the
parsed Journal through application; require mandatory stage root, sequence,
request, and Envelope identity; reconcile retired stages only against exact
committed evidence; surface journal and cleanup failures; preserve every real
request delta; and MUST NOT invent response output. Conflicting or malformed
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

#### Scenario: Append succeeds before journal retirement

- WHEN exact Envelope bytes reach `turns.jsonl` but journal retirement fails
- THEN the operation reports failure and preserves the stage
- AND retry persists retirement and cleanup with exactly one Envelope record

#### Scenario: A small stage follows a large valid record

- WHEN the previous complete Envelope is larger than the staged Envelope by an arbitrary amount and has a different request identity
- THEN reconciliation finds the complete previous record and appends the staged Envelope exactly once
- AND memory use remains bounded independently of the previous record size

#### Scenario: Legacy final record contains concatenated Envelopes

- WHEN the final terminated physical record contains multiple concatenated valid Envelopes
- THEN reconciliation isolates the final Envelope identity and exact byte range
- AND an identical staged Envelope is not appended again

#### Scenario: Whitespace follows the final committed Envelope

- WHEN blank lines, spaces, tabs, or an unterminated whitespace fragment follow an exactly committed staged Envelope
- THEN reconciliation scans past that whitespace and does not duplicate the Envelope
- AND a file containing only whitespace accepts the staged Envelope exactly once

#### Scenario: Final JSON value is not a valid Envelope

- WHEN a terminated tail contains an object without `request_id`, `null`, an invalid request UUID, or non-whitespace residue
- THEN reconciliation rejects the evidence without appending, truncating, retiring the journal entry, or deleting its stage

#### Scenario: Atomic stage-write debris remains

- WHEN exclusive startup recovery finds a regular stage entry named exactly `<sequence>-<request UUID>.tmp-<version-4 temp UUID>`
- THEN it removes only that atomic-write debris and materializes a pending reservation as exactly one incomplete Envelope
- AND any near-miss entry name is retained and causes recovery to fail closed

#### Scenario: Raw generation is a symlink

- WHEN the discovered `turns.jsonl` is a symlink to a file outside the retained session directory
- THEN recovery fails without modifying the target, journal, or stage
- AND successful reconciliation uses one no-follow raw descriptor from classification through mutation

#### Scenario: Terminated tail is malformed

- WHEN `turns.jsonl` ends in newline-terminated bytes that are not valid JSON
- THEN reconciliation reports malformed evidence without appending, truncating, retiring the journal entry, or deleting its stage

#### Scenario: Crash prefix splits a UTF-8 code point

- WHEN the final live bytes are an exact staged-Envelope prefix ending inside a multibyte UTF-8 code point
- THEN recovery repairs the prefix using byte equality and persists the exact Envelope once

#### Scenario: Session traversal encounters a symlink escape

- WHEN a numeric date or session level is a symlink or canonicalizes outside the Session Capture root
- THEN recovery fails before mutating the symlink target or any retained evidence

### Requirement: Immutable generation Sealing

Plant MUST recheck Sealing eligibility under the per-session capture mutex and
atomically detach the eligible raw generation so subsequent captures write a
fresh raw file. The detached generation MUST remain reconstructable and
digest-identified until its zstd frame is committed exactly once. A retry MUST
distinguish an uncommitted destination from the exact post-rename,
pre-detached-removal state. Herdr snapshot append and detachment MUST use the
same session mutex, and Herdr generations MUST use the same recorded base-length
and digest proof before cleanup so a retry cannot duplicate a committed frame.
Scrubbing, detachment, and Sealing MUST retain one no-follow session-directory
descriptor and descriptor-opened regular source, destination, and temporary
files. Every cooperating maintenance process MUST retain an exclusive advisory
session-directory lock from temp recovery through commit and cleanup. Under that
single-owner precondition, rename and cleanup MUST verify that each directory
entry still identifies the retained inode and MUST use directory-relative
operations that do not follow symlinks. Hostile same-account writers that ignore
the advisory lock are outside this contract. Source or merged file data and the
committed destination name MUST be durable before detached evidence is removed,
and the final removal MUST also be directory-durable.
Vaultr MUST provide one validated canonical
generation inventory consumed by Reconstruction, maintenance, and Plant
Sealing. Plant maintenance MUST retain that inventory in a typed selection with
an explicit generation kind; learning, pending-Sealing, and decoding decisions
MUST consume the typed selection without filename- or extension-based
classification. Detached evidence MUST be omitted only after the sealed suffix
at the recorded base decodes to the detached digest. Detached conflicts and
scrubbing, compression, rename, or cleanup failures MUST preserve evidence and
propagate as operational failures. Existing Envelope and concatenated-zstd
generation formats MUST remain readable.

#### Scenario: Capture finishes at the Sealing boundary

- WHEN response completion and generation detachment contend for the same session mutex
- THEN the completed Envelope is either included in the detached generation or keeps that generation ineligible
- AND no Envelope is lost

#### Scenario: Plant restarts with a detached generation

- WHEN Plant restarts after detachment, destination rename, or detached-file removal
- THEN Sealing resumes or recognizes the committed generation without omission or duplication

#### Scenario: Herdr destination rename completes before raw cleanup

- WHEN Herdr Sealing commits the detached frame and crashes before removing the detached raw generation
- THEN retry verifies the exact sealed suffix against the retained base length and digest
- AND it removes the detached evidence without appending the frame again

#### Scenario: A generation entry is substituted before maintenance mutation

- WHEN a static or pre-operation Capture or Herdr source, destination, or temporary directory entry no longer identifies the retained regular-file inode
- THEN detachment or Sealing fails without following a symlink or modifying its target
- AND retained evidence and captured-content redaction in diagnostics are preserved

#### Scenario: Cooperative maintenance processes overlap

- WHEN two Plant processes attempt maintenance in one session directory
- THEN the retained exclusive directory lock serializes temp recovery, commit, and cleanup
- AND one compressor cannot retire the other cooperating compressor's temp evidence

#### Scenario: A previous-version temp remains after upgrade

- WHEN restart finds a regular file named exactly `turns.scrub-tmp`, `turns.jsonl.frame-tmp`, `turns.jsonl.zst-tmp`, `herdr.jsonl.frame-tmp`, or `herdr.jsonl.zst-tmp`
- THEN maintenance removes that descriptor-opened legacy temp under the exclusive session lock
- AND a symlink, non-regular entry, or near-miss name is preserved and causes maintenance to fail closed

#### Scenario: Power fails at the Sealing cleanup boundary

- WHEN the merged destination data and destination rename are durable but detached evidence has not yet been removed
- THEN retry recognizes the exact committed suffix and removes the detached generation without duplication
- AND detached evidence is removed only after the committed destination name is directory-durable

#### Scenario: Compression reports success with corrupt output

- WHEN the compressor exits successfully but the committed suffix does not decode to the detached raw digest
- THEN Sealing reports operational failure and retains the detached evidence
- AND the diagnostic contains locations but no captured content

#### Scenario: Retry finds a different valid frame representation

- WHEN a committed suffix uses different valid zstd frame bytes that decode to the detached raw digest
- THEN retry accepts the content proof and removes the detached evidence without rewriting the destination

#### Scenario: Compression times out

- WHEN zstd exceeds the compression timeout
- THEN Plant kills and reaps the child before returning and cleaning its descriptor-owned temp

#### Scenario: Reconstruction overlaps Sealing

- WHEN sealed, detached, and newer live raw generations coexist
- THEN Reconstruction reads every generation exactly once in chronological order
- AND errors identify only locations rather than captured content

#### Scenario: Sealed length advances without matching evidence

- WHEN a sealed destination is longer than a detached generation's recorded base but its decoded suffix does not match the detached digest
- THEN Reconstruction fails without omitting the detached evidence

#### Scenario: Detached Sealing conflicts

- WHEN the sealed destination cannot be proven uncommitted or exactly committed for the detached generation
- THEN Sealing preserves both generations and reports operational failure
- AND manual compression exits 2 while scheduled compression records failure

#### Scenario: Maintenance selects a capture generation

- WHEN sealed, detached, and raw generation paths are inventoried for learning, coverage, or Sealing
- THEN maintenance carries the validated inventory and selected generation kind through the decision
- AND it does not infer generation kind from a path extension or filename prefix

### Requirement: Complete daemon ownership before recovery

Plant MUST bind and retain both configured harness listeners before capture
recovery or scheduler startup. On any partial collision it MUST release acquired
listeners and exit zero only when both health endpoints identify the expected
complete incumbent Plant; otherwise it MUST fail nonzero. Scheduled compression
MUST run in-process in that listener-owning daemon. Manual compression MUST
acquire and retain both listeners, recover capture persistence, and only then
sweep or refuse to run. A gracefully draining daemon MUST retain both listeners
until no in-flight task can append. Job discovery MUST assign compression an
in-process action, and daemon dispatch MUST use that typed action rather than
executing the manual wrapper.

#### Scenario: Complete incumbent already owns both harnesses

- WHEN another Plant identifies itself correctly on both configured health endpoints
- THEN the losing process exits zero without running recovery

#### Scenario: Only one port is occupied

- WHEN one harness listener is occupied without a complete matching incumbent
- THEN Plant releases any listener it acquired, exits nonzero, and performs no recovery mutation

#### Scenario: Manual compression contends with live capture

- WHEN the listener-owning daemon enters graceful shutdown with an in-flight append and another process requests manual compression
- THEN manual compression exits 2 without scrubbing, renaming, or sealing any generation
- AND the daemon completes the append exactly once

#### Scenario: Scheduled compression is due

- WHEN the compression cadence is due in the listener-owning daemon
- THEN discovery and dispatch select the in-process compression action
- AND that daemon invokes the sweep directly without spawning a child Plant or executing the manual wrapper
- AND any operational failure is recorded as a failed job outcome

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
