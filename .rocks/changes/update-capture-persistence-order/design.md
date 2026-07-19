## Implementation Details

Keep the change inside the existing Capture and Reconstruction modules.

### Preparation journal

`state.json` remains the request delta base and becomes the single atomically
replaced per-session journal. In addition to the existing request body, it stores
private `next_sequence`, `next_to_drain`, and pending request halves. A legacy
file without ordering fields loads with its delta base preserved and an empty
journal; sequencing starts lazily on the first new reservation.

A `capture.rs`-private per-session async mutex, keyed by canonical Session
Capture root plus session id, serializes journal mutation, stage publication,
and eligible draining. It is never held across upstream response streaming.
Cross-process locking is not added without evidence that multiple Plant
instances write the same vault concurrently.

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

For each eligible sequence, while holding the session mutex:

1. append the exact staged Envelope to `turns.jsonl`;
2. atomically advance `next_to_drain` and remove the pending request half;
3. delete the private stage file.

Persisted line order remains the Envelope contract. Preparation sequence is not
added to the public Envelope schema; `request_id` is the idempotency identity.
No global duplicate scan is introduced.

### Startup recovery

Plant recovers every staged session before binding proxy ports or permitting
Sealing:

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
- Missing ordering fields default to an empty journal only when no private stage
  backlog exists.

`compress_sweep` consults a narrow crate-private Capture predicate and skips a
raw generation with open reservations or completed stages. Journal and staging
details do not become a public watchdog/session state.

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
  without inventing responses, and read historical concatenated records.
- Non-Goals: a new trait, public Interface, Module, Adapter, generic queue,
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
  moves are not a current requirement; recovery reports the mismatch.
- Additional journal writes add local I/O. Per-request sync is intentionally
  excluded to avoid an unmeasured hot-path durability cost.

## Migration Plan

- Do not eagerly rewrite existing Session Captures or `state.json` files.
- Initialize ordering fields lazily while preserving the existing delta base.
- Read historical concatenated records through Reconstruction compatibility.
- Before rolling back to an older Plant, drain all journals and verify the
  private stage tree is empty; older code may then ignore additive state fields.

## Open Questions

None. The Herdr Grilling councils resolved ordering, staging, recovery, failure,
compatibility, and non-goal decisions.
