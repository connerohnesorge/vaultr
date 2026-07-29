# Capture Stewardship Design

## ADRs

### ADR-0001: Capture owns preparation-ordered durable persistence and generation mutation

Capture reserves response sequence in preparation order because each request's
delta base advances at preparation time and Reconstruction applies persisted
records in file order. One strict per-session journal therefore owns both delta
advancement and sequence state. Completed responses are durably published to
private staging before success, then byte-exactly reconciled and drained in
sequence; startup recovery uses the same transaction. A slow earlier response
may block later draining, but cannot leave a later completion only in memory or
publish an unscrubbed stage in the Git-backed Capture tree.

The capture subsystem also owns the complete readiness, detachment, Sealing,
and recovery transaction through one retained no-follow session-directory
boundary and cooperative lock. Sweep may select inventory and policy, but MUST
NOT become a second generation mutator or filesystem owner. Scheduled
compression runs inside the process retaining both capture listeners; manual
compression must first retain both listeners and recover before it sweeps.
The scheduler's attempt fence is owned by `plant-agent-jobs/design.md`
ADR-0002; this decision owns only capture persistence and generation state.

### ADR-0002: Normalize WebSocket turns into existing capture envelopes

A transport-specific WebSocket envelope would permanently split
reconstruction, telemetry, scrubbing, and coverage semantics. Plant instead
represents each Codex WebSocket `response.create` turn as the existing request
JSON and SSE response body, records semantic response status 200, and expands
validated response-id deltas into complete logical history. This loses durable
frame-boundary metadata but preserves every response event as JSON and keeps
all downstream consumers transport-independent.

### ADR-0003: Learn state is recorded explicitly; Git and frontmatter cannot supply it

Learn state looks redundant beside Git history and each learning file's `sources:`
frontmatter, and the cheap answer to a multi-writer hazard is to delete the shared file.
Measurement refutes both routes.

Git records diffs, and a skipped pass produces none: 1,124 sessions were examined and
produced no learning file, hence no commit, hence no Git evidence that the work happened
at all. Git also cannot attribute a learner — all 1,653 vault commits in the last 30 days
carry one author, and learn outputs are swept into generic 30-minute autocommits whose
timestamps track the sweep, not the pass. A learning file's most recent commit is
routinely an unrelated refactor.

Frontmatter fares no better. No learning file records its learner, and learner does not
correlate with session harness (the Codex learner mines Claude sessions more often than
Codex ones). In 215 sessions one learner learned while the other skipped; reconstructed
from frontmatter that is indistinguishable from "the second learner never ran", and
because sealing requires every learner, reconstruction would manufacture 215
permanently unsealable captures.

Decision: keep learn state as explicit records and fix their storage shape.
Consequence: learn records are authoritative and must be preserved by any future change
to session directories or to `learnings/`.

### ADR-0004: Immutable per-pass learn records rather than one mutable record per writer

A mutable record per `(session, learner, host)` would hold file count to 3,201 instead
of 3,363 — a 5% difference — and both layouts are conflict-free across hosts, since the
host appears in the filename either way. The distinction is the write contract.

A mutable record must be truncated and replaced in place, so its write path is
destructive: a crash or a stale read at the wrong moment destroys a real prior pass,
which is the failure class this change exists to eliminate. An immutable record is
create-if-absent, so no code path can destroy a pass and atomicity is a property of the
filesystem rather than of the writer's care.

Immutability also removes the need to describe when a second record is legitimate. An
earlier draft tried to enforce one record per `(session, learner)` and would have
rejected the documented resumed-session case outright, breaking normal Learn semantics.
Under per-pass records that question does not arise.

Decision: immutable, create-only, one file per pass. Consequence: a session's learn
history accumulates. Nothing consumes that history — every consumer folds latest-wins —
so it costs storage only, at ~1 record per session directory.
