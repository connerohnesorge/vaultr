## Implementation Details

### Shard addressing

```
learnings/.ledger/<short-host>-<learner>.jsonl   # e.g. CB14957-claude.jsonl
learnings/.ledger.jsonl                          # legacy, read-only
```

`<short-host>` matches the existing host convention: the first dot-segment of the
hostname, the same value `jobs.rs:131-133` compares against the `jobs/.hostname`
marker. `<learner>` is the existing `Harness::ledger_label()` (`claude` / `codex`).

One writer per shard follows from the pair: a learner job is serialized on its own
host by Plant's per-job lock, and different hosts write different files.

### Reader

New `crates/vaultr/src/ledger.rs`, built to fold N files from the start so that
adding or removing a writer never changes a reader:

```rust
pub struct Pass { pub processed_at: u64, pub outcome: String }
/// Every shard under learnings/.ledger/ plus the legacy learnings/.ledger.jsonl.
pub fn shard_paths(content_root: &Path) -> Result<Vec<PathBuf>>;
/// session_id -> learner -> latest Pass, latest processed_at winning.
pub fn load(content_root: &Path) -> Result<HashMap<String, HashMap<String, Pass>>>;
```

Replaces two independent parsers:

- `crates/plant/src/sweep.rs:15` `ledger_latest()` — already folds into
  `HashMap<session_id, max processed_at>` filtered to one learner, so N files is a
  loop over paths, not a redesign. Feeds `ready_to_seal`, `stuck_captures`,
  `eligible_candidates`.
- `crates/vaultr/src/validate.rs:321` — the per-line validation walk;
  `ledger_path()` at `:90` becomes `shard_paths()`.

### Row format

Unchanged except that the `learnings` array is no longer written. `ledger_latest`
reads only `session_id`, `learner`, `processed_at`; validation checks only that
`session_id` parses. Readers MUST still tolerate the field on the 1,542 legacy rows
that carry it.

## Context

Written from a measured investigation, not a hypothesis. Live counts at the time of
writing: 3,333 rows, 1,791 `skipped` (54%), 1,326 claude / 1,489 codex rows across
3,181 distinct `(session, learner)` pairs, 215 sessions where the two learners
disagree, 0 learning files carrying a `learner:` field.

## Goals / Non-Goals

- Goal: no two concurrent writers can ever share a ledger file.
- Goal: disjoint hosts merge in git without conflict, so `commit-push.30m.sh` cannot
  wedge on the ledger and stall Session Capture sync.
- Goal: one reader, removing duplicated ledger knowledge rather than adding a copy.
- Non-Goal: migrating the 3,333 existing rows. The legacy file is read in place.
- Non-Goal: surfacing learn state to agents or to `vaultr session list`. Real value,
  separate change.
- Non-Goal: feeding recorded skips into learn dispatch. A sealed-then-resumed session
  has genuinely new content, and `Learn.md` warns that an old entry does not mean
  already-ledgered.

## Decisions

- Shard by `(host, learner)`, not by host. Both learners run on the same host today
  (`learn.3h.sh`, `learn-codex.15m.sh`), so host-only sharding leaves the original
  same-machine incident possible.
- Legacy file read in place rather than migrated. A migration rewrites 3,333 rows
  under exactly the concurrent-writer hazard being removed.
- `merge=union` in `.gitattributes` was rejected. The recorded incident was a clean
  sequential commit, not a merge, so a union driver would never have fired.

## ADRs

### ADR-0001: Keep the learn ledger; it is not derivable from vault content

The ledger looks redundant next to each learning file's `sources:` frontmatter, and
the cheap response to a multi-writer hazard is to delete the shared file. Measurement
refutes that. 1,791 of 3,333 rows are `skipped` passes — negative results recorded
nowhere else, since a session that produced nothing leaves no file to carry
frontmatter. No learning records its learner, and learner is uncorrelated with session
harness (the Codex learner mines Claude sessions more often than Codex ones), so
per-learner state cannot be recovered either. In 215 sessions one learner learned and
the other skipped; derived from frontmatter that is byte-identical to "that learner
never ran", and because sealing requires every learner, reconstruction would
manufacture 215 permanently unsealable captures.

Decision: keep the ledger and fix its storage shape instead. Consequence: the ledger
stays authoritative for learn state and must be preserved by any future change to
`learnings/`.

## Risks / Trade-offs

- The append-only gate at `Learn.md:43` hardcodes
  `git diff --cached -- learnings/.ledger.jsonl`. Once rows move, that path stops
  changing, `grep -q '^-{'` matches nothing, the leading `!` inverts to true, and the
  gate **passes vacuously forever**. → The sibling `.dotfiles` change must move the
  gate to the shard glob or retire it deliberately; single-writer shards make its
  original lost-update scenario unreachable, but a vacuous check is worse than none.
- A reader that globs a directory can silently see zero shards where it previously
  read one file, reporting everything as unlearned and re-dispatching the corpus.
  → `shard_paths` returns an error when `learnings/.ledger/` is unreadable, and the
  legacy file is always included, so an empty result is distinguishable from a failure.
- Hostname collision across the fleet would put two writers on one shard.
  → Allocator hostnames must be unique; asserted when the allocator leg lands.

## Migration Plan

1. Land the reader folding legacy + shards, with writers unchanged. Behavior identical.
2. Land the sibling `.dotfiles` write-path and gate change so new rows go to shards.
3. Legacy file stops growing and is retained read-only. No rewrite, no backfill.

Rollback is step 2 alone: revert the writers and rows resume landing in the legacy
file, which the reader still folds.

## Open Questions

- Should the legacy file eventually be folded into a shard and removed, or kept
  read-only indefinitely? Not required by this change.
