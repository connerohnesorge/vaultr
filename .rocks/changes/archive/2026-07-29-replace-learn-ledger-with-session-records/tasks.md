## 1. Reader

- [ ] 1.1 Add `crates/vaultr/src/learn.rs` with `session_passes()` folding `learn-*.json` records in one session directory, and `legacy_index()` reading the frozen `learnings/.ledger.jsonl` once
- [ ] 1.2 Parse the learner by matching the `learn-<learner>-` prefix against the known learner set, never by splitting on `-` — hostnames contain dashes and splitting misattributes the record
- [ ] 1.3 Reject a record whose prefix names no known learner, rather than skipping it silently
- [ ] 1.4 Fold latest `processed_at` per learner across per-pass records and legacy rows; a legacy row with no learner counts as Claude
- [ ] 1.5 Error on an unreadable session directory so an empty fold is never confused with a read failure
- [ ] 1.6 Register `pub mod learn;` in `crates/vaultr/src/lib.rs`
- [ ] 1.7 Unit-test: two passes for one learner, latest wins; legacy-only session counted; legacy row superseded by a newer record; unknown-learner filename rejected; a host containing dashes parses correctly

## 2. Replace the independent parsers

- [ ] 2.1 Rewrite `crates/plant/src/sweep.rs` `ledger_latest()` to consume the shared reader, preserving its `HashMap<session_id, max processed_at>` per-learner shape
- [ ] 2.2 Fold learn records into `session_generations()` (`sweep.rs:218-224`), which already walks every session directory, so no separate traversal is added
- [ ] 2.3 Replace the `crates/vaultr/src/validate.rs` ledger walk (`ledger_path` at `:90`, validation at `:321`) with per-record validation plus the legacy file, naming the offending path in each finding
- [ ] 2.4 Confirm `ready_to_seal`, `stuck_captures`, and `eligible_candidates` compile unchanged against the new reader

## 3. Proof

- [ ] 3.1 Capture a `plant sessions stuck --age 24h` baseline immediately before testing and assert old and new readers agree **against that same content snapshot** — do not compare against a number recorded earlier, the live counts drift between runs
- [ ] 3.2 Prove a resumed session records a second pass and every earlier record stays byte-for-byte unchanged
- [ ] 3.3 Prove two hosts' records for one session and learner merge in git with no conflict
- [ ] 3.4 Prove a malformed record is reported against its own path and does not silently count as a pass
- [ ] 3.5 Prove the legacy ledger still counts unmigrated, including its 138 duplicate keys and 518 learner-less rows
- [ ] 3.6 `cargo test --workspace`

## 4. Sequencing and coordination

- [ ] 4.1 This change lands first. Verify the installed Plant recognises records before any writer moves — a writer ahead of the reader makes every new pass invisible and re-dispatches the corpus
- [ ] 4.2 Sibling `.dotfiles` change: append helper writes `O_EXCL` per-pass records, `Learn.md` names no ledger path, `verify.5m.sh` and `reflect.2h.sh` re-pointed off the frozen ledger
- [ ] 4.3 Leave `learnings/.ledger.jsonl` in place, read-only. No migration, no backfill, no rewrite
- [ ] 4.4 On archive, promote ADR-0001 and ADR-0002 into `.rocks/specs/capture-stewardship/design.md`, since `cnb rocks archive` merges `spec.md` only
