## 1. Shared reader

- [ ] 1.1 Add `crates/vaultr/src/ledger.rs` with `shard_paths()` and `load()`, folding every shard under `learnings/.ledger/` plus the legacy `learnings/.ledger.jsonl`, latest `processed_at` winning per `(session, learner)`
- [ ] 1.2 Make `shard_paths()` fail loudly when `learnings/.ledger/` exists but is unreadable, so an empty fold is never confused with a read failure
- [ ] 1.3 Register `pub mod ledger;` in `crates/vaultr/src/lib.rs`
- [ ] 1.4 Unit-test the fold: two shards for one session, latest wins; legacy-only rows counted; unreadable shard directory errors

## 2. Replace the independent parsers

- [ ] 2.1 Rewrite `crates/plant/src/sweep.rs` `ledger_latest()` to call `vaultr::ledger::load()`, keeping its `HashMap<session_id, max processed_at>` per-learner shape
- [ ] 2.2 Replace `crates/vaultr/src/validate.rs` `ledger_path()` with `shard_paths()` and walk every shard, naming the offending shard in each finding
- [ ] 2.3 Confirm `ready_to_seal`, `stuck_captures`, and `eligible_candidates` compile unchanged against the new reader

## 3. Row format

- [ ] 3.1 Stop writing the `learnings` array in new rows; keep readers tolerant of it on the 1,542 legacy rows that carry it

## 4. Proof

- [ ] 4.1 Assert the sharded reader reproduces today's live classification exactly — `seal-blocked=0`, `half-learned:claude=1`, `half-learned:codex=310`, `unlearned=206`, `sub-threshold=137`, `job-capture=0` — before any writer moves
- [ ] 4.2 Prove two shards written by different hosts merge in git with no conflict
- [ ] 4.3 Prove two learners on one host write disjoint shards and neither can replace the other's rows
- [ ] 4.4 `cargo test --workspace`

## 5. Coordination

- [ ] 5.1 Sibling `.dotfiles` change: move the write path in `skills/Vault/Workflows/Learn.md` to shard addressing, ship the append helper so no agent hardcodes a path
- [ ] 5.2 Sibling `.dotfiles` change: move or deliberately retire the append-only gate at `Learn.md:43` — left as-is it passes vacuously once rows move
- [ ] 5.3 On archive, promote ADR-0001 into `.rocks/specs/capture-stewardship/design.md`, since `cnb rocks archive` merges `spec.md` only
