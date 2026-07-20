## Implementation Details

Keep capture construction and response handling in `capture.rs`, journal and
stage state in its private persistence submodule, and shared generation truth
in Vaultr. One neutral private `capture/session_fs.rs` owns the retained
directory, no-follow open, advisory lock, rename, unlink, and directory-sync
primitives. A sibling `capture/generation.rs` owns the complete
readiness-to-detach-to-Sealing transaction, Capture and Herdr maintenance, and
the compressor lifecycle. Sweep owns inventory selection and policy only; it
does not own generation mutation or a second filesystem boundary.

### Preparation journal

`state.json` remains the request delta base and becomes the single atomically
replaced per-session journal. A private persistence module owns one strict
`Journal` loader, stage representation, ordered commit, and recovery inventory.
The loader explicitly validates the legacy schema, harness, session identity,
request body, and optional thread identity. When `capture_order` exists,
`next_sequence`, `next_to_drain`, `pending`, and `root` are all mandatory and
their bounds and request identities are validated. A valid legacy file without
ordering fields keeps its delta base and initializes sequencing lazily on the
first new reservation.

A `capture.rs`-private per-session async mutex, keyed by canonical Session
Capture root plus session id, serializes journal mutation, stage publication,
eligible draining, and raw-generation detachment. It is never held across
upstream response streaming. Live capture persistence adds no separate
cross-process lock; complete ownership of both existing listeners is that
process-ownership boundary. Scheduled compression therefore runs directly
inside the listener-owning daemon. A manual `compress once` must first acquire
and retain both listeners, recover persistence state, and only then sweep, so
it refuses while that daemon is active. Maintenance also retains an exclusive
advisory flock on each session directory from temp recovery through commit and
cleanup, serializing cooperating Plant processes at the filesystem boundary.

### Completed stage files

Completed Envelopes are atomically published as one file per sequence under:

```text
~/.local/state/plant/capture-staging/
  <sha256(canonical-session-root)>/
    <session-id>/
      <sequence>-<request-id>.json
```

Local stage metadata also records the canonical Session Capture root so a hash
collision or path mismatch fails recovery. Completed stages never live in the
Git-backed Session Capture tree before security scrub.

An interrupted atomic stage publication can leave only
`<sequence>-<request-uuid>.tmp-<temp-v4-uuid>`. During exclusive startup
recovery, Plant removes files matching exactly that writer-owned grammar before
materializing the still-pending reservation as incomplete. Every near-miss
entry remains fail-closed.

`finish_capture` returns success once its completed Envelope is durably staged,
even if an earlier live sequence prevents draining. A stage-write failure or an
eligible drain failure returns an error and retains the stage for recovery.

### Ordered drain

Live draining and restart recovery both call one byte-exact `commit_stage`
transaction. For each eligible sequence, while holding the session mutex:

1. reconcile or append the exact serialized Envelope bytes to `turns.jsonl`;
2. atomically advance `next_to_drain` and remove the pending request half;
3. delete the private stage file.

An exact crash prefix can end at any byte, including inside a multibyte UTF-8
code point; no lossy text conversion participates in reconciliation. Plant
opens the session directory without following its final component and opens
`turns.jsonl` relative to that descriptor with `O_NOFOLLOW`. Classification,
range comparison, truncation, and append all use that same raw-generation
descriptor.

A typed backward tail classifier scans fixed-size chunks independently of the
staged record size and skips all trailing whitespace-only records or fragments;
`Blank` means the entire file contains only whitespace. Terminated records use
the same concatenated-value stream decoder as Reconstruction, deserialize only
request identity, retain the final Envelope's byte range, and reject malformed
residue or any value without a UUID `request_id`. Exact identity and crash
prefixes compare descriptor ranges chunkwise, so even a large record is never
duplicated into a full buffer or second `Value` DOM. A valid different request
permits append, an identical request requires byte-exact Envelope bytes, and
only an exact staged prefix permits repair. Malformed terminated or conflicting
evidence fails without mutation. If the append succeeds but the journal write
fails, or the journal write succeeds but stage cleanup fails, retained evidence
makes the next attempt converge to one record while propagating the failed
operation.

Persisted line order remains the Envelope contract. Preparation sequence is not
added to the public Envelope schema; `request_id` is the idempotency identity.
No global duplicate scan is introduced.

### Daemon ownership and startup recovery

Plant binds and retains both proxy listeners before recovery. A partial bind is
released immediately. An address collision exits zero only when both health
endpoints identify the expected Plant harness and upstream; a partial or
unrecognized owner fails nonzero. Recovery and the scheduler therefore run only
in the process that owns both harnesses. On graceful shutdown, accept loops stop
accepting but retain their bound listeners until the in-flight capture drain
ends, so a replacement or manual process cannot mutate generations while an old
append can still finish.

Recovery inventories only the staging hash for the canonical current root and
the exact Session Capture paths returned by the shared explicit-error walker.
Numeric date and session levels must be real directories, never symlinks, and
every canonical discovered session must remain beneath the canonical root.
Each journal is parsed once into a retained `RecoverySession`; validation and
application consume that same value rather than reopening mutable evidence.

- Every pre-restart reservation without a completed matching stage becomes an
  explicit incomplete Envelope at its reserved position, preserving the real
  request delta with `response.complete=false` and no invented output.
- Completed stages are interleaved at their reserved positions and the entire
  journal drains before any new reservation is accepted.
- Exact atomic-write temp debris is removed before its pending reservation is
  materialized once as incomplete; every other staging entry remains an error.
- A leftover stage is already committed only when the final complete persisted
  Envelope has the same `request_id` and exactly equals the stage.
- If the final live bytes are an exact prefix of the retained staged Envelope,
  recovery may truncate only that incomplete tail to the prior newline and
  append the full staged record.
- Same-id content mismatches, non-tail conflicts, vault-identity mismatches, or
  stages without a readable matching journal leave persisted bytes unchanged
  and fail startup.
- A valid legacy journal may omit `capture_order`; a present `capture_order`
  must contain every field and satisfy all bounds and request identities.
- Journals and stages must have valid object shapes and matching root, sequence,
  request, Session, and Envelope identity. Missing and malformed state are never
  treated as permissive defaults.
- A discovered journal is recovered at its exact path; metadata is never used to
  invent a replacement dated directory.
- A retired stage is removed only when it exactly matches the final committed
  Envelope. Cleanup errors fail startup while leaving evidence retryable.
- Abandoned incomplete Envelopes use the same exact-complete/prefix-tail
  reconciliation as completed stages.

### Immutable generation Sealing

`compress_sweep` selects a typed inventory and enters one narrow crate-private
Capture transaction. The capture-owned transaction acquires the session mutex
and retained directory flock before asking persistence for a strict,
non-mutating readiness verdict. It then scrubs the closed raw file, hashes it,
and renames it to
`turns.jsonl.sealing-<prior-zstd-length>-<sha256>`. New captures then create a
fresh `turns.jsonl` without touching the detached generation. Persistence never
calls sweep or mutates a generation; sweep and Herdr call only the narrow
capture-owned transaction and sidecar-append APIs.

Vaultr owns one canonical capture-generation filename grammar. Maintenance and
Plant Sealing consume its validated path inventory; Reconstruction materializes
a distinct private retained-handle snapshot from the same grammar because a
path inventory cannot remain coherent across a Sealing rename. Plant sweep
retains the complete path inventory in a typed session-generation selection
with an explicit raw, sealed, or detached kind. Learning eligibility,
pending-Sealing selection, and capture decoding use that kind and never
re-derive evidence type from a filename or extension. Sealing regenerates the
detached generation's zstd frame. The prior destination length identifies the
commit boundary: a destination at that length is uncommitted; a longer
destination is the
post-rename/pre-detached-removal state only when the canonical decoded-suffix
proof matches the detached raw digest. This content proof deliberately accepts
different valid zstd frame representations for the same generation. Any other
state fails without deleting evidence. Reconstruction reuses the same decoded
suffix proof against its retained sealed descriptor. Sealed length alone is
never proof.

The private Reconstruction snapshot opens the session directory
`O_DIRECTORY|O_NOFOLLOW`, takes a blocking shared advisory flock with
interrupted calls retried, enumerates the canonical generation grammar, and
opens every recognized entry relative to that directory with
`O_RDONLY|O_NOFOLLOW|O_NONBLOCK`. While locked it requires regular files,
rejects duplicate device/inode identities, retains each fstat length, verifies
the detached digest from its retained descriptor, reopens every entry to
revalidate device/inode/type, and proves that the lexical directory pathname
still identifies the retained directory. It then drops the shared lock and
streams only the retained handles, with every reader bounded to its captured
length. A live append therefore belongs to the next snapshot, while rename and
unlink cannot invalidate the current one. Sealed, detached, and raw ordering is
unchanged.

Detached conflicts and scrubbing, compression, rename, or cleanup failures
preserve the evidence and propagate as operational failures. Manual compression
exits 2 and the daemon scheduler records `failed`; neither reports “nothing to
seal” or success.

Herdr snapshot appends take the same short session lock used by capture
persistence. Sealing detaches `herdr.jsonl` into the same
base-length/digest-identified transaction shape before compression. The shared
exact-once commit primitive decodes and hashes an already-renamed destination
suffix before removing detached evidence, so a crash between destination rename
and cleanup cannot append the Herdr content twice.

Capture and Herdr scrubbing, detachment, and Sealing retain one no-follow
session-directory descriptor and open every source, destination, and
unpredictably named temporary relative to it. The retained directory also owns
an exclusive advisory flock through temp recovery, hashing, compression,
rename, digest proof, and cleanup. Under that cooperative single-owner
precondition, no second Plant process can create or retire another compressor's
temporary entry. Same-account writers that ignore the advisory lock are outside
this contract; device/inode checks reject static or pre-operation substitutions
but do not claim atomic rejection of a hostile swap in the syscall gap.
`SessionDirectory` explicitly applies `LOCK_UN` before its retained file closes:
a concurrent fork can briefly inherit the same open-file description before
exec honors `O_CLOEXEC`, but that duplicate cannot extend a completed
transaction's lock.

Hashing, compression input/output, suffix comparison, rename, and cleanup use
retained regular-file descriptors. Directory-relative rename and unlink do not
follow symlinks. Each source or merged file is synced before rename, the
directory is synced after every rename, and the committed destination is synced
before detached evidence is unlinked and the directory is synced again. Thus a
power-loss boundary can retain both names or only the committed destination,
but cannot durably lose both source evidence and the destination name.

Restart cleanup recognizes only the current hidden UUIDv4 temp grammar plus the
five exact deterministic names emitted by the immediately preceding version:
`turns.scrub-tmp`, `turns.jsonl.frame-tmp`, `turns.jsonl.zst-tmp`,
`herdr.frame-tmp`, and `herdr.zst-tmp`. The Herdr names are the actual
`Path::with_extension("frame-tmp" | "zst-tmp")` results for the former
`herdr.jsonl` source. The mistakenly inferred `herdr.jsonl.frame-tmp` and
`herdr.jsonl.zst-tmp` names are unrecognized evidence, not migration debris.
Cleanup opens each exact entry no-follow and removes it only when regular;
symlinks, non-regular entries, forbidden names, and near misses fail closed. A
timed-out zstd child is explicitly killed and reaped before its retained output
is cleaned.

All orchestration callers share one subprocess runner that accepts an already
configured Tokio command without replacing its stdin, stdout, or stderr. Jobs
configure null stdin and both output pipes; zstd configures retained source and
frame descriptors plus only piped stderr. One absolute deadline covers the
direct child wait and every optional drain concurrently. A timeout, wait error,
or drain error first drops the joined drain future, then calls `start_kill` and
waits unconditionally to reap the retained direct child before returning typed
end and cleanup diagnostics. If a background descendant inherits an output
pipe after the direct child exits, the still-open pipe cannot extend the
deadline. Detached descendants are not owned or terminated. Callers must await
the runner to completion; external task abortion retains only `kill_on_drop` as
a cancellation backstop.

The detached filename and location diagnostics contain no captured content.
Legacy Envelope files and concatenated zstd frames remain unchanged.

Session Index updates and Herdr snapshots run once at durable stage acceptance,
matching current response-finish timing. Their failures are logged separately
and do not reclassify an accepted stage as lost.

### Reconstruction

Deepen the existing Reconstruction path:

- Take a private shared-locked, no-follow, regular-file descriptor snapshot of
  sealed, detached, and raw generations before streaming.
- Bound every segment to its retained fstat length, reject duplicate inodes,
  and never reopen a generation pathname after the snapshot.
- Ignore whitespace-only terminated records.
- Recover every complete concatenated JSON Envelope value from a terminated
  non-whitespace record.
- Return an error naming only segment (`sealed` or `raw`) and one-based record
  number when terminated residue cannot form complete Envelopes.
- Ignore an unterminated final fragment only for a live raw `turns.jsonl`.
- Fail on incomplete or malformed trailing content in a sealed capture.

Embedded SSE parsing and legacy Envelope decoding remain separate contracts.
Issue #16 mixed-generation sibling selection keeps its semantics but now
consumes the stable descriptor snapshot.

## Context

Request history deltas advance at `prepare_capture`, but today independently
spawned response tasks call `finish_capture` and append at stream completion.
Reconstruction applies deltas in persisted file order.

The real sealed Session Capture
`09d3ed80-c721-4c3b-bbc4-4adea7120d4f` contains 725 complete Envelope objects
but current Reconstruction reports 723. One terminated physical record contains
two concatenated complete Envelopes followed by a blank record. The capture also
contains many `observed_at` inversions, demonstrating that completion order and
preparation order differ in practice.

## Goals / Non-Goals

- Goals: preserve delta lineage, prevent concurrent append interleaving, retain
  completed evidence across Plant process crashes, recover abandoned requests
  without inventing responses, seal each immutable generation exactly once,
  require complete daemon ownership before recovery, and read historical
  concatenated records.
- Non-Goals: a new trait, public Interface, Adapter, generic queue,
  Envelope sequence field, live gap timeout, public watchdog state,
  cross-process locking for live capture append beyond complete listener
  ownership, vault-move migration, or clone/cache optimization.
- Non-Goals: per-request `fsync`/`sync_data` and host power-loss durability
  outside the immutable Sealing commit boundary.
- Non-Goals: changing permissive embedded SSE parsing, retained legacy decode
  branches, learner eligibility policy, or issue #16 sibling discovery.
- Non-Goals: defending against hostile same-account filesystem writers that
  ignore the cooperative session-directory flock.

## Decisions

- Persist in preparation order, not completion/channel-arrival order, because
  delta bases advance during preparation and Reconstruction applies file order.
- Use the existing `state.json` as one journal so sequence reservation and delta
  advancement cannot drift across a two-file transaction.
- Keep completed response stages in Plant private state so `git add -A sessions`
  cannot publish unsanitized staged responses before scrub.
- Perform eager startup recovery; lazy recovery would leave completed evidence
  invisible for dormant sessions.
- Keep scheduled compression in the listener owner and make manual compression
  compete for both listeners, because a process-local session mutex cannot
  coordinate a child process. Job discovery assigns compression a typed
  in-process action; scheduled dispatch never executes its manual wrapper.
- Keep readiness validation in persistence, descriptor mechanics in one neutral
  session-filesystem module, and every generation mutation in the capture-owned
  generation module. Sweep can select policy without becoming a second
  persistence or filesystem owner.
- Centralize generation parsing in Vaultr so every consumer makes the same
  evidence-preserving decision.
- Do not abandon live gaps by timeout. Normal EOF, stream error, or disconnect
  already completes the existing path, while long model pauses are valid.
- Cover Plant process crashes using completed writes and atomic replacement.
  Immutable Sealing additionally syncs its destination-name and source-cleanup
  ordering; per-request journal power-loss durability remains unmeasured.

## Risks / Trade-offs

- Preparation ordering can create head-of-line blocking behind a slow stream.
  Later completions remain durably staged rather than held in memory or lost.
- Corrupt journal/stage combinations can prevent Plant startup. Failing closed
  preserves evidence and is preferable to silently guessing delta order.
- A moved vault leaves private stages keyed to its old canonical path. Vault
  moves are not a current requirement; current-root recovery does not process
  the other root's evidence.
- Additional journal writes add local I/O. Per-request sync is intentionally
  excluded to avoid an unmeasured hot-path durability cost.
- Scrubbing and hashing run while the session mutex is held so the renamed
  generation is immutable. Response streaming remains unlocked; only completion
  for that session waits at the final persistence boundary.

## Migration Plan

- Do not eagerly rewrite existing Session Captures or `state.json` files.
- Initialize ordering fields lazily while preserving the existing delta base.
- Read historical concatenated records through Reconstruction compatibility.
- Resume any existing detached generation before considering a newer raw file.
- Before rolling back to an older Plant, drain all journals and verify the
  private stage tree is empty; older code may then ignore additive state fields.

## Open Questions

None. The Herdr Grilling councils resolved ordering, staging, recovery, failure,
compatibility, and non-goal decisions.
