# Capture Stewardship Specification

## Requirements

### Requirement: Stuck Session Capture detection

Plant MUST classify every pending unsealed Session Capture generation idle for
at least a configurable age (default 24 hours) by its ownership and
learn-ledger state: `job-capture` when Plant registered it as a Cultivation Job
self-capture, `seal-blocked` when every learner has ledgered the selected
generation yet it remains unsealed, `half-learned` naming the missing learner
when only a strict subset of learners ledgered the selected generation,
`unlearned` when no learner ledgered it and it passes the learn substance gate,
and `sub-threshold` when no learner ledgered it and it falls below the substance
gate. A resumed raw generation beside older sealed or detached evidence MUST
count a learner only when its latest ledger entry postdates that prior
generation boundary. Detection MUST be read-only against Session Captures and
MUST propagate maintenance inventory failures rather than returning a partial
classification.

#### Scenario: Sealing is failing on a fully learned capture

- WHEN a pending Session Capture generation idle beyond the age threshold is ledgered by every learner
- THEN it is reported as `seal-blocked`

#### Scenario: One learner never processed a capture

- WHEN a pending Session Capture generation idle beyond the age threshold is ledgered by only one learner
- THEN it is reported as `half-learned` naming the missing learner

#### Scenario: Learn never picked up a substantive capture

- WHEN a pending Session Capture generation idle beyond the age threshold has no ledger entry and passes the substance gate
- THEN it is reported as `unlearned`

#### Scenario: Capture below the substance gate

- WHEN a pending Session Capture generation idle beyond the age threshold has no ledger entry and falls below the substance gate
- THEN it is reported as `sub-threshold`

#### Scenario: Cultivation Job self-capture is informational

- WHEN an idle pending Session Capture is registered as a Plant Cultivation Job self-capture
- THEN it is reported as `job-capture`
- AND it is not actionable for learn dispatch or watchdog failure

#### Scenario: Resumed generation needs fresh learning

- WHEN a raw generation exists beside an older sealed or detached generation and a learner's latest ledger entry does not postdate that boundary
- THEN that learner is missing for the raw generation's classification

#### Scenario: Active and sealed captures are exempt

- WHEN a pending Session Capture generation was modified within the age threshold, or a Session Capture is already sealed with no pending generation
- THEN it is not reported

#### Scenario: Stuck inventory is incomplete

- WHEN a missing, unreadable, or unsafe traversal level prevents a complete maintenance inventory
- THEN detection fails instead of returning the captures found before the error

### Requirement: Watchdog Cultivation Job

Plant MUST run a `watchdog` Cultivation Job every 6 hours that invokes the
shared stuck detection and records the outcome: `failed` with deterministic
per-state counts when any `seal-blocked`, `half-learned`, or `unlearned`
capture exists, otherwise `success`. Sub-threshold and `job-capture` captures
MUST be counted in the recorded detail but MUST NOT fail the job. An inventory
failure MUST fail the job and MUST NOT be recorded as a healthy pipeline. The
job launches no agent pane and writes nothing to Session Captures.

#### Scenario: Actionable stuck captures exist

- WHEN detection finds at least one seal-blocked, half-learned, or unlearned capture
- THEN the job logs one line per stuck capture and records `failed` with deterministic per-state counts

#### Scenario: Only informational captures exist

- WHEN detection finds only sub-threshold or job-capture entries
- THEN the job records `success` with each informational count in the detail

#### Scenario: Pipeline is healthy

- WHEN detection finds nothing stuck
- THEN the job records `success`

#### Scenario: Watchdog inventory fails

- WHEN stuck detection cannot complete the maintenance inventory
- THEN the command exits 2 and the watchdog records `failed`
- AND it does not emit a healthy summary from partial evidence

### Requirement: Manual stuck inspection subcommand

`plant sessions stuck [--age <duration>]` MUST print one line per stuck
capture (state, session id, idle time) using the same classification as the
watchdog job, and MUST exit 0 when no actionable stuck captures exist and 1
when any actionable capture exists.

#### Scenario: Operator inspects a stuck pipeline

- WHEN `plant sessions stuck` runs against a vault with actionable stuck captures
- THEN each stuck capture prints with its state and idle time and the process exits 1

#### Scenario: Operator inspects a healthy pipeline

- WHEN `plant sessions stuck` runs against a vault with no actionable stuck captures
- THEN the process exits 0

### Requirement: Observation-window capture coverage audit

Plant MUST provide a read-only coverage audit for a single Session Capture that
compares captured Envelope response `request-id`s against the harness
transcript's distinct assistant `requestId`s, restricted to Plant's observation
window. The window start MUST be the earliest captured Envelope `observed_at`,
falling back to the Capture's meta `original_start` when no Envelope exists.
The audit MUST stream every retained sealed, detached, and live raw Envelope
generation in chronological generation order using Reconstruction's canonical
traversal, including complete concatenated records and its live-tail/error
semantics, no matter which canonical sibling is selected. Detached evidence
MUST contribute after the sealed base unless the same retained sealed handle
proves that the suffix from the detached generation's recorded base through
that handle's captured length decodes to the detached raw digest; only that
proof MAY omit the detached handle as already committed. It MUST stream the
native transcript and retain memory bounded by the largest record plus
comparison ID sets. Capture evidence that cannot be opened, decoded, or parsed
under Reconstruction's rules MUST fail the audit.
Native `requestId`s whose first transcript occurrence precedes the window start
MUST be reported as out-of-scope carryover (not as missing capture), and the
audit MUST report in-window coverage as captured over in-window native together
with the list of residual missing `request-id`s. Harness support MUST be derived
from Envelope truth. Codex Captures and Claude windows with zero in-window
comparable native IDs, including nonempty all-carryover transcripts, MUST fail
explicitly and MUST NOT print a percentage. The audit MUST NOT mutate any
Session Capture or transcript.

#### Scenario: Resumed session with pre-proxy history

- WHEN a resumed Session Capture's transcript contains assistant `requestId`s
  timestamped before the earliest captured Envelope's `observed_at`
- THEN those pre-window `requestId`s are reported as out-of-scope carryover
- AND in-window coverage counts only native `requestId`s at or after the window start

#### Scenario: Complete in-window capture

- WHEN every in-window native `requestId` has a matching captured Envelope `request-id`
- THEN coverage is reported as 100% with an empty residual missing list

#### Scenario: Resumed capture has sealed, detached, and live raw generations

- WHEN a Capture has a sealed base, an unproven detached generation, and a newer live raw generation
- THEN all three generations contribute in that order regardless of which canonical sibling is selected
- AND complete concatenated records and the final live tail follow Reconstruction's behavior

#### Scenario: Detached generation is already committed

- WHEN the same retained sealed handle proves that its decoded suffix from the detached base has the detached raw digest
- THEN the committed suffix contributes through the sealed handle
- AND the detached handle is not visited a second time

#### Scenario: Capture evidence is malformed or unreadable

- WHEN any selected Capture generation cannot be opened, decoded, or parsed under Reconstruction's rules
- THEN the audit fails instead of omitting that evidence

#### Scenario: Large capture and transcript

- WHEN release coverage audits hold record size and ID cardinality constant while
  decoded Capture evidence scales through at least 919 MiB
- THEN it streams both inputs with memory bounded by the largest record plus comparison ID sets

#### Scenario: Genuine in-window gap

- WHEN an in-window native `requestId` has no matching captured Envelope `request-id`
- THEN it appears in the residual missing list and lowers the reported coverage

#### Scenario: Codex has no comparable native denominator

- WHEN Envelope truth identifies the Capture as Codex
- THEN the audit fails with explicit unsupported or no-comparable-ID text
- AND it does not print a percentage

#### Scenario: Claude has zero in-window comparable native IDs

- WHEN a Claude transcript contains no comparable assistant `requestId` at or
  after the observation-window start, including when every ID is carryover
- THEN the audit fails with explicit no-comparable-ID text
- AND it does not print a percentage

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
the advisory lock are outside this contract. The retained directory owner MUST
explicitly unlock before closing its file descriptor so a concurrent
fork-before-exec duplicate cannot extend a completed transaction. Source or
merged file data and the committed destination name MUST be durable before
detached evidence is removed, and the final removal MUST also be
directory-durable.
Vaultr MUST provide one canonical generation grammar. Maintenance and Plant
Sealing MUST consume its validated path inventory, while Reconstruction MUST
materialize a private retained-handle snapshot from that same grammar under a
cooperative shared session-directory flock. Plant maintenance MUST retain its
inventory in a typed selection with an explicit generation kind; learning,
pending-Sealing, and decoding decisions MUST consume the typed selection
without filename- or extension-based classification. Reconstruction MUST open
every generation no-follow relative to its retained directory, require regular
and pairwise-distinct device/inode identities, verify detached digest evidence
from the retained handle, and bound every segment to the length captured from
that handle. Detached evidence MUST be omitted only after the sealed suffix at
the recorded base through the captured sealed length decodes from the same
retained sealed handle to the detached digest. Detached conflicts and
scrubbing, compression, rename, or cleanup failures MUST preserve evidence and
propagate as operational failures. Existing Envelope and concatenated-zstd
generation formats MUST remain readable. Capture MUST own one
readiness-to-detach-to-Sealing API: persistence MAY return only a strict,
non-mutating readiness verdict, the neutral session-filesystem module MUST own
the low-level descriptor operations, and sweep and Herdr MUST use narrow
capture-owned APIs rather than mutating generations or rebuilding locks.

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

#### Scenario: A subprocess fork inherits the locked directory description

- WHEN a concurrent fork temporarily duplicates the session-directory open-file description before exec applies `O_CLOEXEC`
- THEN dropping the transaction owner explicitly unlocks before closing its descriptor
- AND the inherited duplicate cannot delay the next maintenance owner

#### Scenario: A previous-version temp remains after upgrade

- WHEN restart finds a regular file named exactly `turns.scrub-tmp`, `turns.jsonl.frame-tmp`, `turns.jsonl.zst-tmp`, `herdr.frame-tmp`, or `herdr.zst-tmp`
- THEN maintenance removes that descriptor-opened legacy temp under the exclusive session lock
- AND `herdr.jsonl.frame-tmp`, `herdr.jsonl.zst-tmp`, any symlink or non-regular entry, and any other near-miss name are preserved and cause maintenance to fail closed

#### Scenario: Capture maintenance ownership is entered

- WHEN sweep selects a typed generation inventory or Herdr needs to append a sidecar snapshot
- THEN it calls a narrow capture-owned API without opening, renaming, sealing, or locking the generation itself
- AND strict journal and stage readiness is validated before any lower-level Capture detachment can occur

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

#### Scenario: A shared subprocess exceeds its complete deadline

- WHEN a direct child remains alive past the deadline or exits while a background descendant retains stdout or stderr
- THEN one absolute deadline bounds the child wait and every caller-requested output drain
- AND Plant drops the drains, explicitly kills and reaps the direct child, and returns without waiting for the inherited pipe
- AND it does not claim to terminate detached descendants

#### Scenario: Callers configure subprocess descriptors

- WHEN a job requests null stdin and two pipes or zstd requests retained file stdin, retained file stdout, and one stderr pipe
- THEN the shared runner preserves that preconfigured standard I/O and drains only pipe handles returned by the spawned child
- AND exit 75, spawn failure, timeout, wait failure, output failure, and cleanup diagnostics remain distinguishable

#### Scenario: Reconstruction overlaps Sealing

- WHEN sealed, detached, and newer live raw generations coexist
- THEN Reconstruction retains a shared-locked no-follow snapshot before Sealing can replace the destination and remove detached evidence
- AND it releases the shared lock before streaming retained handles exactly once in chronological order
- AND errors identify only locations rather than captured content

#### Scenario: Live raw grows after its reconstruction snapshot

- WHEN a live raw append completes after Reconstruction captures the retained raw descriptor length
- THEN the current snapshot reads only through that captured length with live-tail tolerance
- AND a fresh Reconstruction observes the appended Envelope

#### Scenario: Unsafe reconstruction generation entries

- WHEN a canonical generation name resolves to a symlink, FIFO, directory, other non-regular entry, or an inode already retained under another generation name
- THEN Reconstruction rejects the snapshot without following or decoding that entry

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

### Requirement: Fallible Session Capture maintenance inventory

Vaultr MUST walk the Session Capture root through numeric year, month, and day
directories and real session directories with explicit errors. An existing
empty root MUST produce a complete empty inventory. A missing root, unreadable
root or numeric date level, unreadable directory entry, symlinked date or
session level, canonical escape, or unreadable/invalid generation MUST fail the
inventory instead of returning a partial or empty result. Plant eligibility,
stuck detection, compression, and startup recovery MUST propagate that failure
without performing work based on an incomplete inventory. Non-numeric
root/year/month/day entries MUST remain outside the Session Capture walk.

Plant maintenance commands MUST distinguish domain outcomes from operational
failures. `plant sessions eligible` MUST exit 0 only when it prints a selected
batch, 1 when a complete inventory contains no eligible sessions, and 2 when
inventory or claim publication fails. `plant sessions stuck` MUST exit 0 when a
complete inventory has no actionable stuck capture, 1 when it reports an
actionable stuck capture, and 2 when inventory fails. `plant compress once`
MUST exit 0 after a successful no-op or sweep, 1 for an operational Sealing
failure after ownership and recovery, and 2 when listener ownership, recovery,
or inventory fails. Learn-job wrappers consuming `sessions eligible` MUST
convert only status 1 to benign no-work success; they MUST propagate status 2
without invoking `plant agent run`.

#### Scenario: Existing root has no Session Captures

- WHEN the Session Capture root exists and contains no numeric dated sessions
- THEN maintenance receives a successful empty inventory

#### Scenario: Session Capture root is missing

- WHEN the configured Session Capture root does not exist
- THEN maintenance fails with a location-bearing inventory error
- AND it does not treat the missing root as no work

#### Scenario: Numeric date traversal is unreadable

- WHEN an otherwise eligible numeric year, month, or day directory cannot be read
- THEN the walk fails without returning sessions found before the error
- AND eligibility, stuck detection, compression, and recovery propagate the failure

#### Scenario: Unsafe traversal level is present

- WHEN a numeric date or session level is a symlink or canonicalizes outside the Session Capture root
- THEN the walk fails before reading or mutating the target

#### Scenario: Non-date content shares the root

- WHEN metadata, notes, backups, or temporary directories do not form numeric year/month/day levels
- THEN maintenance ignores them without treating them as Session Captures

#### Scenario: Complete eligibility inventory has no work

- WHEN `plant sessions eligible` successfully inventories the root but selects no sessions
- THEN it exits 1 and prints no paths
- AND a learn wrapper may convert that status to a successful no-work attempt

#### Scenario: Eligibility inventory or claim fails

- WHEN `plant sessions eligible` cannot complete inventory or durably publish its claim
- THEN it exits 2 and prints no unclaimed paths
- AND each learn wrapper propagates status 2 without launching an agent

#### Scenario: Stuck inspection cannot complete inventory

- WHEN `plant sessions stuck` encounters a maintenance inventory failure
- THEN it exits 2 instead of reporting a healthy or actionable domain result

#### Scenario: Compression cannot enter its ownership boundary

- WHEN `plant compress once` cannot retain both listeners, recover capture state, or complete inventory
- THEN it exits 2 without reporting a successful no-op or mutating from partial evidence

### Requirement: Native Codex Responses WebSocket capture

Plant MUST accept a valid WebSocket upgrade only for a Codex `GET /responses`
request whose path the configured adapter captures for HTTP `POST`, dial the corresponding upstream
`ws://` or `wss://` endpoint with the request's end-to-end credentials, and
relay data, control, and close frames bidirectionally with bounded
backpressure while forwarding upstream application handshake metadata. Plant
MUST omit `generate=false` prewarm exchanges from durable turn capture and MUST
expand a validated `previous_response_id` request's incremental `input` with
the prior normalized request and response items before using the existing
request-body delta encoder. For every other sequential top-level
`response.create` request, Plant MUST normalize the request JSON and upstream
JSON text events into the existing request-body and SSE response envelope, MUST
prefer fresh frame `client_metadata` over immutable upgrade metadata for turn
identity, and MUST finish that envelope as complete only when the exact top-level
`response.completed` event reaches the downstream client, and MUST finish an
active turn as transport-incomplete when the connection closes, fails, or
receives a new turn before completion. A broken response-id chain or overlapping
turn, malformed client frame, or unrecognized client application frame MUST
disable further capture on that connection rather than persist truncated or
cross-turn evidence. If a turn is active, Plant MUST finish it as
transport-incomplete before disabling capture. Capture or telemetry failure
MUST NOT interrupt otherwise valid proxy traffic. Existing HTTP Responses
proxying and non-Codex upgrade rejection MUST remain unchanged.

#### Scenario: Native WebSocket turn completes

- WHEN Codex upgrades `GET /responses` and sends a `response.create` JSON text frame
- THEN Plant relays the unchanged request and response event frames through a credentialed upstream WebSocket
- AND Plant persists one complete existing-format envelope whose request omits the transport discriminator and whose response is the equivalent SSE event stream

#### Scenario: One connection carries sequential turns

- WHEN Codex sends a second `response.create` only after the first turn's exact `response.completed` event
- THEN Plant persists each turn as a separate envelope in preparation order without closing the WebSocket
- AND each envelope uses that frame's fresh turn identity even when upgrade metadata names the earlier turn

#### Scenario: Prewarm precedes an incremental turn

- WHEN Codex completes a `generate=false` prewarm and sends a turn with the matching `previous_response_id` and only incremental `input`
- THEN Plant does not persist the prewarm as a user turn
- AND the captured turn contains the complete logical request history including prior response items

#### Scenario: Response-id chain is not trustworthy

- WHEN an incremental request does not match the response id and serialized context observed on the connection
- THEN Plant relays the frames but disables capture on that connection without persisting truncated history

#### Scenario: Turn is interrupted

- WHEN either WebSocket closes or relay fails before the active turn receives exact `response.completed`
- THEN Plant persists the active turn as transport-incomplete with every response event received before interruption

#### Scenario: Active turn receives ambiguous client data

- WHEN a malformed, unrecognized, or binary client application frame arrives before the active turn completes
- THEN Plant relays the frame unchanged, finishes the active turn as transport-incomplete, and disables capture on that connection
- AND later upstream events cannot be mixed into that turn's evidence

#### Scenario: Control and close lifecycle

- WHEN either peer sends ping, pong, or close control frames
- THEN Plant services or relays the protocol lifecycle without unbounded buffering
- AND Plant's shutdown drain owns the upgraded connection task

#### Scenario: Upstream handshake metadata

- WHEN the upstream upgrade returns Codex model, capability, catalog, or turn-state application headers
- THEN Plant forwards those headers in the downstream 101 response without copying hop-by-hop or extension-negotiation headers

#### Scenario: Upstream handshake is rejected

- WHEN the upstream rejects the WebSocket handshake with an HTTP response such as 426 or 401
- THEN Plant preserves that response's status and safe end-to-end headers so Codex can perform HTTP fallback or credential recovery
- AND Plant omits the potentially partial or transfer-encoded rejection body
- AND Plant returns 502 only for dial or protocol failures without an upstream HTTP response

#### Scenario: Capture preparation fails

- WHEN valid WebSocket traffic cannot be parsed or prepared for capture
- THEN Plant still relays that traffic without mixing its evidence into another turn

#### Scenario: Existing transport behavior remains stable

- WHEN a Codex request uses HTTP SSE or a non-Codex request attempts an upgrade
- THEN Plant preserves the existing HTTP SSE behavior and rejects the unsupported upgrade
