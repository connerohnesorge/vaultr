## Implementation Details

Keep capture construction and response handling in `capture.rs`, persistence
state in its private concrete submodule, and shared generation truth in Vaultr.

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
upstream response streaming. Cross-process locking is not added; complete
ownership of both existing listeners is the process-ownership boundary.
Scheduled compression therefore runs directly inside the listener-owning
daemon. A manual `compress once` must first acquire and retain both listeners,
recover persistence state, and only then sweep, so it refuses while that daemon
is active.

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
code point; no lossy text conversion participates in reconciliation. A typed
backward tail classifier scans independently of the staged record size and
distinguishes blank, valid terminated, malformed terminated, and unterminated
evidence. A valid different request permits append, an identical request
requires byte-exact equality, and only an exact staged prefix permits repair.
Malformed terminated or conflicting evidence fails without mutation. If the
append succeeds but the journal write fails, or the journal write succeeds but
stage cleanup fails, retained evidence makes the next attempt converge to one
record while propagating the failed operation.

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

`compress_sweep` enters a narrow crate-private Capture transaction. Under the
session mutex it rechecks the strict journal and stage backlog, scrubs the
closed raw file, hashes it, and renames it to
`turns.jsonl.sealing-<prior-zstd-length>-<sha256>`. New captures then create a
fresh `turns.jsonl` without touching the detached generation.

Vaultr owns one canonical capture-generation inventory that validates the
sealed, raw, and digest-identified detached paths. Reconstruction, maintenance,
and Plant Sealing all consume that inventory. Plant sweep retains the complete
inventory in a typed session-generation selection with an explicit raw, sealed,
or detached kind. Learning eligibility, pending-Sealing selection, and capture
decoding use that kind and never re-derive evidence type from a filename or
extension. Sealing regenerates the detached generation's zstd frame. The prior
destination length identifies the commit boundary: a destination at that
length is uncommitted; a destination whose exact suffix is the regenerated
frame is the post-rename/pre-detached-removal state and only needs cleanup; any
other state fails without deleting evidence. Reconstruction reads sealed,
detached, and new live raw generations in order. It omits detached bytes only
after decoding the sealed suffix at the recorded base and proving its raw
digest; sealed length alone is never proof.

Detached conflicts and scrubbing, compression, rename, or cleanup failures
preserve the evidence and propagate as operational failures. Manual compression
exits 2 and the daemon scheduler records `failed`; neither reports “nothing to
seal” or success.

The detached filename and location diagnostics contain no captured content.
Legacy Envelope files and concatenated zstd frames remain unchanged.

Session Index updates and Herdr snapshots run once at durable stage acceptance,
matching current response-finish timing. Their failures are logged separately
and do not reclassify an accepted stage as lost.

### Reconstruction

Deepen the existing Reconstruction path:

- Ignore whitespace-only terminated records.
- Recover every complete concatenated JSON Envelope value from a terminated
  non-whitespace record.
- Return an error naming only segment (`sealed` or `raw`) and one-based record
  number when terminated residue cannot form complete Envelopes.
- Ignore an unterminated final fragment only for a live raw `turns.jsonl`.
- Fail on incomplete or malformed trailing content in a sealed capture.

Embedded SSE parsing, legacy Envelope decoding, and issue #16 mixed-generation
sibling selection remain separate contracts.

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
  cross-process locking, vault-move migration, or clone/cache optimization.
- Non-Goals: per-request `fsync`/`sync_data` and host power-loss durability.
- Non-Goals: changing permissive embedded SSE parsing, retained legacy decode
  branches, Vault Learn generation selection, or issue #16 sibling discovery.

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
- Centralize generation parsing in Vaultr so every consumer makes the same
  evidence-preserving decision.
- Do not abandon live gaps by timeout. Normal EOF, stream error, or disconnect
  already completes the existing path, while long model pauses are valid.
- Cover Plant process crashes using completed writes and atomic replacement.
  Host power-loss durability is a separate, unmeasured requirement.

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
