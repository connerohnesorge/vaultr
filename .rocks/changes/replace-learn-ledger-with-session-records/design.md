## Implementation Details

### Record layout

```
sessions/YYYY/MM/DD/<sid>/learn-<learner>-<host>-<YYYYMMDDTHHMMSSZ>.json
{"processed_at":"2026-07-28T22:40:13Z","outcome":"learned","learnings":["slug-a"],"notes":"…"}
```

`<learner>` is an existing `Harness::ledger_label()` (`claude` / `codex`). `<host>` is
the short hostname, the same value `jobs.rs:131-133` compares against the
`jobs/.hostname` marker. The trailing timestamp exists only to make the name unique.

Parse the learner by matching the `learn-<learner>-` **prefix** against the known
learner set, never by splitting on `-`: hostnames contain dashes (`allocator-vm-1`) and
naive splitting would misattribute the record. A filename whose prefix names no known
learner is rejected, not skipped, so a typo cannot silently drop a pass.

No `session_id` and no `learner` field in the content: the path carries both, so a
record that disagrees with its location is unrepresentable rather than validated
against. `learnings` stays — `verify.5m.sh` reads it. `notes` stays — 1,313 legacy rows
carry it and it is where a skip states its reason.

Writes are `O_EXCL` create-only. There is no code path that opens a learn record for
writing, so no pass can destroy another, and a same-second collision fails loudly
instead of silently overwriting.

### Reader

New `crates/vaultr/src/learn.rs`:

```rust
pub struct Pass { pub processed_at: u64, pub outcome: String, pub learnings: Vec<String> }
/// learner -> latest Pass, from one session directory's learn-*.json records.
pub fn session_passes(session_dir: &Path) -> Result<HashMap<String, Pass>>;
/// session_id -> learner -> latest Pass, from the frozen legacy ledger. Read once.
pub fn legacy_index(content_root: &Path) -> Result<HashMap<String, HashMap<String, Pass>>>;
```

Callers fold the two, latest `processed_at` winning per learner.

Replaces two independent parsers:

- `crates/plant/src/sweep.rs:15` `ledger_latest()` — feeds `ready_to_seal`,
  `stuck_captures`, `eligible_candidates`. Its `HashMap<session_id, max processed_at>`
  per-learner shape is preserved; only the source changes.
- `crates/vaultr/src/validate.rs:321` — the per-line validation walk; `ledger_path()`
  at `:90` gives way to per-record validation plus the legacy file.

`session_generations()` (`sweep.rs:218-224`) already walks all 2,818 session
directories and loads `CaptureGenerations` per session. Learn records fold in there, so
the sweep's traversal count drops from three (walk + `.meta` + ledger) to two.

## Context

Measured, not assumed. 3,363 legacy rows over 1,775 distinct sessions; 1,772 of those
sessions have a capture directory. 3,201 distinct `(session, learner)` pairs, so 162
passes (≈4%) are repeats — the resumed-session case is real but rare. 138 raw duplicate
keys, 141 once a missing learner normalises to Claude. 1,552 rows carry a non-empty
`learnings` array. 2,818 session directories on disk.

## Goals / Non-Goals

- Goal: no shared mutable learn-state file, so the lost-update and merge-conflict
  classes cease to exist rather than being guarded against.
- Goal: one reader, folded into a walk that already happens.
- Goal: a resumed capture records a pass with no special rule.
- Non-Goal: migrating the 3,363 legacy rows. The file is frozen and read in place.
- Non-Goal: surfacing learn state to agents or `vaultr session list`. Real value,
  separate change.
- Non-Goal: feeding recorded skips into learn dispatch. A sealed-then-resumed session
  has genuinely new content, and `Learn.md` warns an old entry does not mean
  already-recorded.

## Decisions

- Records live in the session's own directory, not in `.meta`. `.meta` is authoritative
  for identity and discovery, learn state is neither; and Plant rewrites `.meta`
  continuously during live sessions, which is precisely the two-writer arrangement that
  erased 43 `dropped_turns` counters in 14 days.
- The writing host is in the filename, not the content, so cross-host records never
  collide on a path and no host-uniqueness precondition is needed for correctness.
- `merge=union` in `.gitattributes` was rejected. The recorded incident was a clean
  sequential commit, not a merge, so a union driver would never have fired.
- No `PreToolUse` block on hand-written learn records. Such a hook was scoped while learn
  state was an aggregate file a bad write could corrupt; with per-pass immutable records
  a stray file can only add a pass, which validation rejects if malformed. Enforcing the
  helper is a style concern, not an integrity one.

## ADRs

### ADR-0001: Learn state is recorded explicitly; Git and frontmatter cannot supply it

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

### ADR-0002: Immutable per-pass records rather than one mutable record per writer

A mutable record per `(session, learner, host)` would hold file count to 3,201 instead
of 3,363 — a 5% difference — and both layouts are conflict-free across hosts, since the
host appears in the filename either way. The distinction is the write contract.

A mutable record must be truncated and replaced in place, so its write path is
destructive: a crash or a stale read at the wrong moment destroys a real prior pass,
which is the failure class this change exists to eliminate. An immutable record is
create-if-absent, so no code path can destroy a pass and atomicity is a property of the
filesystem rather than of the writer's care.

Immutability also removes the need to describe when a second record is legitimate. An
earlier draft of this change tried to enforce one record per `(session, learner)` and
would have rejected the documented resumed-session case outright, breaking normal
Learn semantics. Under per-pass records that question does not arise.

Decision: immutable, create-only, one file per pass. Consequence: a session's learn
history accumulates. Nothing consumes that history — every consumer folds latest-wins —
so it costs storage only, at ~1 record per session directory.

## Risks / Trade-offs

- A reader that globs a directory can see zero records where legacy rows previously
  existed and report a whole corpus unlearned, re-dispatching thousands of agent runs.
  → The legacy ledger is always folded in and is never rewritten, so pre-cutover state
  survives any fault in the new path; an unreadable session directory is an error, not
  an empty fold.
- Learn records land under `sessions/`, which `Learn.md` tells learn agents not to
  commit. → The sibling change must make the helper commit its own record explicitly;
  `commit-push.30m.sh` runs `git add -A`, so an uncommitted record still syncs within
  30 minutes, but the learn pass should not depend on that.
- Reading learn state now depends on locating a session's directory, so a session whose
  capture directory is absent has nowhere to record a pass. Measured: 3 of 1,775.
  → Those keep their legacy rows, which are never removed.

## Migration Plan

1. Land the reader folding legacy rows plus per-pass records, with writers unchanged.
   Behaviour identical; classification must be byte-identical against a fixed snapshot.
2. Land the sibling `.dotfiles` change so new passes write records.
3. The legacy ledger stops growing and is retained read-only. No rewrite, no backfill.

Rollback is step 2 alone: revert the writers and passes resume landing in the legacy
file, which the reader still folds.

## Open Questions

- Should legacy rows eventually be materialised as per-pass records and the file
  deleted? Not required by this change; the fold makes it unnecessary.
