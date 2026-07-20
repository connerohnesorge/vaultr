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
atomically persist the exact ordered in-progress batch plus a portable total
size and SHA-256 identity for each file's exact bounded bytes before launch,
and derive a stable Plant Agent Run idempotency key from that content-bound
claim. Hydration MUST reproduce the claimed mtime, size, and digest before
launch. A retry MUST resume the persisted claim. Only a machine-readable Plant
result confirming a
durably recorded `Succeeded` or `Failed` outcome MAY atomically advance the
frontier through the claim and clear it. Retryable, operationally failed,
missing, malformed, or otherwise indeterminate results MUST retain the claim
and key. New Door state-directory levels MUST be durably linked before a lock,
claim, migration, or outcome is published. Lock owner metadata MUST be complete
and fsynced before the canonical lock path becomes visible. An incomplete
canonical lock from the legacy publisher MUST be boundedly reread and then left
intact while the Door fails closed; it MUST NOT be reclaimed by a running Door.
Its removal requires an explicit offline migration after verifying no legacy
Door process exists. Reclaiming a complete lock whose owner is dead MUST occur
under one kernel-backed per-door recovery lock, with the observed PID and token
reread and matched while guarded before unlink and directory fsync. If that
guard is unavailable or the owner changed, the Door MUST fail closed or retry
without unlinking the canonical lock. A given file MUST never launch a second
agent run. Legacy scalar and cursor migrations MUST reject negative zero,
frontiers with no representable finite successor, and absolute or traversing
paths. Every migrated value MUST pass canonical v2 parsing before it is saved
or used. A migrated v1 in-progress claim MUST retain its historical Plant key
and MUST durably bind portable content identities under the Door lock before
its first post-migration launch.

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

#### Scenario: Lock publisher crashes

- WHEN a Door process crashes while preparing lock owner metadata
- THEN it cannot leave an incomplete canonical lock
- AND any temporary lock inode it leaves is ignored

#### Scenario: Legacy lock publisher is incomplete

- WHEN a Door sees an incomplete canonical lock that a shipped legacy publisher may still be populating
- THEN it boundedly rereads the lock and fails closed if complete owner metadata does not appear
- AND it leaves the canonical lock intact for explicit offline migration
- AND no agent session is launched

#### Scenario: Two processes observe one stale owner

- WHEN two Door processes read the same complete lock whose recorded owner is dead
- THEN stale-owner verification and unlink occur under the same kernel-backed recovery lock
- AND a delayed contender cannot unlink a successor's live canonical lock
- AND only one process can claim and launch the batch

#### Scenario: Files share a timestamp

- WHEN `z.md` is durably processed and `a.md` lands later with the same mtime
- THEN the durable seen-path set includes `z.md` without treating the timestamp as closed
- AND `a.md` is included on the next evaluation without re-firing `z.md`

#### Scenario: Door crashes before launch

- WHEN a door crashes after persisting its claim but before calling Plant
- THEN the next invocation resumes the same ordered claim and idempotency key
- AND the batch launches once

#### Scenario: Claimed path is replaced without changing mtime

- WHEN a claimed regular file is replaced with different bounded bytes at the same path and mtime before retry
- THEN hydration detects the size or digest mismatch and fails closed
- AND the persisted claim and key remain unchanged
- AND no Plant agent run is launched for the replacement

#### Scenario: Door crashes after launch

- WHEN a door crashes after Plant durably records the launch outcome but before the frontier save
- THEN the next invocation reuses the persisted key and Plant's durable prior outcome
- AND no second Herdr workspace is created

#### Scenario: Shipped state is migrated

- WHEN a Door reads a shipped scalar `hwm` state
- THEN it atomically persists a valid v2 timestamp frontier under the Door lock before evaluation
- AND it closes the entire legacy high-water timestamp at the next representable timestamp so no file can fire twice

#### Scenario: Corrupt legacy state cannot launch during migration

- WHEN a shipped scalar contains negative zero or no finite successor, or a v1 cursor path is absolute or traversing
- THEN migration fails closed without replacing the legacy state
- AND canonical v2 parsing occurs before any migrated value is saved or used
- AND no agent session is launched

#### Scenario: Shipped v1 cursor is migrated

- WHEN a Door reads a shipped v1 `(mtime,path)` cursor state
- THEN it atomically persists a v2 frontier at that timestamp with the cursor path as its durable closed-through boundary
- AND paths sorting after the boundary remain eligible while paths through the boundary cannot fire twice
- AND the boundary and same-timestamp seen set remain until a claim advances the frontier to a newer timestamp
- AND a v1 in-progress claim retains its original Plant idempotency key

### Requirement: Ingestion-only watch roots

The library MUST select exactly one allowlisted ingestion root — a path written
only by sync jobs — for each watch glob. It MUST reject traversal and
canonicalize the selected root. Every scanned or hydrated file MUST be opened
with nonblocking no-follow semantics and MUST be verified as a regular file
beneath that root. Scan eligibility against the timestamp frontier MUST be
decided before content is read. At most 65,536 bytes of content MUST then be
read from that same descriptor. Pre-read and post-read descriptor metadata
MUST match for device, inode, size, mtime, and ctime; any change MUST fail
before launch. A real path outside the selected root, including a symlink
escape, MUST be rejected before launch. A door whose watch glob falls outside
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

#### Scenario: Unrelated symlink escape is outside the watch

- WHEN an escaping symlink does not match the Door's watch glob
- THEN no-follow glob scanning excludes it from evaluation
- AND matching safe files remain eligible

#### Scenario: Match is replaced after open

- WHEN a matching pathname is replaced by an escaping symlink after the Door opens it
- THEN metadata and content are read only from the already-opened no-follow descriptor
- AND outside-root content does not enter the prompt or launch

#### Scenario: Match changes during its descriptor read

- WHEN an in-place writer changes a matching file after the Door's first descriptor stat
- THEN the post-read stat detects the identity or stability change and the Door fails before launch
- AND a later stable evaluation can process the file once

#### Scenario: Large match is bounded

- WHEN a matching file exceeds 65,536 bytes
- THEN the Door reads and exposes only its first 65,536 bytes
- AND it does not read or decode the remainder

#### Scenario: Matching FIFO cannot block

- WHEN the watch glob matches a FIFO or another non-regular filesystem object
- THEN the nonblocking open and descriptor type check return without reading it
- AND Door evaluation does not block or launch for that object

#### Scenario: Processed path is behind the frontier

- WHEN a scanned regular file is not eligible against the durable frontier
- THEN the Door closes its descriptor without reading content

#### Scenario: Hidden paths require an explicit hidden segment

- WHEN an ordinary glob such as `mail/*.md` is evaluated
- THEN hidden matches such as `mail/.secret.md` are excluded
- AND a glob explicitly naming `mail/.door/` can evaluate that hidden ingestion path

### Requirement: Mail projection produces immutable Door inputs

Before the mail Door evaluates, its mail-specific producer MUST stream only
newline-terminated records from the append-only daily JSONL capture under one
run-wide byte budget and a per-line cap. It MUST open and close one source at a
time and MUST defer its single durable state commit until every source passes.
Machine-local, ignored `mail/.door/` state MUST durably record a version,
initialization flag, and each source's relative path, device, inode, and EOF
offset. `.door` and `summons` MUST be real non-symlink directories. Projector
state and existing artifacts MUST be opened with no-follow, nonblocking
semantics, verified as regular files, and read under small explicit size
bounds before JSON parsing. The first true run MUST snapshot every existing
source EOF and publish zero artifacts; a source first discovered after
initialization MUST start at offset zero. A tracked source that disappears,
truncates, changes inode, or no longer names the opened regular file MUST fail
closed without advancing any source offset.

The producer MUST accept only inbox records with nonempty Graph `id` and
trimmed `internetMessageId`, and MUST match the literal summon only in
`subject`, `bodyPreview`, or `body.content`. It MUST durably publish one
non-overwriting artifact keyed by `sha256(trimmed internetMessageId)`, retaining
the Graph ID and complete message, before atomically advancing the source
offset. A retry that finds the artifact MUST parse and verify the same trimmed
Internet Message ID without rewriting or touching it; other message fields,
including a Graph ID changed by a move, MUST NOT create a conflict for that
stable duplicate. The mail Door MUST watch only
`mail/.door/summons/*.json`, with no content filter.

#### Scenario: Historical bootstrap does not replay

- WHEN the projector initializes against daily JSONL files containing historical summons
- THEN it durably snapshots their current device, inode, and EOF offsets
- AND it creates no artifacts or agent launch

#### Scenario: New daily source starts at zero

- WHEN a new daily JSONL source appears after initialization
- THEN the projector begins that source at offset zero
- AND each complete newly appended summon remains eligible

#### Scenario: Summon follows a large daily prefix

- WHEN a summoned message is appended after more than 65,536 bytes of earlier daily JSONL
- THEN bounded incremental projection publishes its immutable artifact
- AND the mail Door can launch one agent for that message

#### Scenario: Artifact precedes offset advancement

- WHEN the producer crashes after fsyncing an artifact but before its atomic offset save
- THEN retry verifies the artifact's trimmed Internet Message ID
- AND it advances the offset without changing the artifact inode, bytes, or mtime
- AND the mail Door launches that immutable path at most once

#### Scenario: Graph ID changes for a stable duplicate

- WHEN a replay carries the same trimmed Internet Message ID with a different Graph ID
- THEN the existing artifact is accepted without rewrite or conflict
- AND no duplicate artifact is published

#### Scenario: Source is replaced or truncated

- WHEN a tracked JSONL source is replaced, disappears, or shrinks below its durable offset
- THEN projection fails closed without changing any durable offset
- AND the mail Door does not evaluate newly projected input

#### Scenario: Partial or over-budget data

- WHEN a source ends in a partial line or the run-wide byte budget is exhausted
- THEN no offset advances past incomplete or unread bytes
- AND a later run can resume from the durable offset

#### Scenario: Projector runtime files are hostile filesystem objects

- WHEN `.door` or `summons` is a symlink, or state or an existing artifact is a symlink, FIFO, non-regular file, or over its read bound
- THEN projection rejects the object without blocking, following it, or parsing unbounded bytes
- AND no source offset advances

#### Scenario: Many historical daily sources

- WHEN projection evaluates many tracked daily JSONL files
- THEN it holds at most one source descriptor at a time
- AND it publishes the combined next offsets only after every source passes

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
tagged `AgentRunReceipt`, deriving durability and the expected process exit
status from the receipt variant. It MUST NOT reconstruct independently supplied
state, durability, and exit-code fields or reimplement any part of the Herdr
lifecycle owned by Plant.

#### Scenario: Non-durable result does not advance the claim

- WHEN `plant agent run` reports a retryable or indeterminate result or does not emit a valid result
- THEN the door's timestamp frontier does not advance
- AND the in-progress claim and idempotency key remain durable
- AND the same files are eligible on the next run
