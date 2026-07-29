# Change: Store the learn ledger as one shard per writer

## Why

`vault/learnings/.ledger.jsonl` is a single shared mutable file appended by learn
agents. Two learners already lost each other's rows on one machine: Codex commit
`9510bfc` appended ten rows, and thirty-six seconds later Claude commit `1bca018`
rewrote the file from a stale snapshot, deleting all ten. Plant then rescheduled
those sessions and their captures became unsealable.

The shipped `guard-vault-learn-ledger-append-only` gate diffs staged content against
local HEAD, so it is single-machine by construction. Learn is moving onto the nix
allocator fleet, which makes the file multi-host: each host's gate passes, the push
goes non-fast-forward, and `commit-push.30m.sh` falls to `git merge` with no `union`
driver in `.gitattributes`. That is a hard conflict, and that job pushes Session
Captures and the Session Index too — so one ledger collision wedges all vault sync.

The ledger cannot be dropped or reconstructed. 1,791 of 3,333 rows record `skipped`
passes — negative results stored nowhere else. No learning file records its learner,
and learner does not correlate with session harness. 215 sessions have one learner
learning and the other skipping; rebuilt from frontmatter those are indistinguishable
from "never ran", and since sealing requires every learner, that would manufacture 215
permanently unsealable captures.

Sharding by host alone is insufficient: `learn.3h.sh` and `learn-codex.15m.sh` both run
on the same host, so both learners would still share one file and the original incident
would remain possible.

## What Changes

- Store learn-ledger rows as one shard per `(host, learner)` writer under
  `learnings/.ledger/`, so every shard has exactly one writer and disjoint shards merge
  in git without conflict.
- Read the ledger through one shared reader in the `vaultr` crate that folds all shards,
  replacing the two independent parsers in `sweep.rs` and `validate.rs`.
- Continue reading the legacy `learnings/.ledger.jsonl` as a read-only shard so existing
  rows keep counting with no migration step.
- Drop the `learnings` array from newly written rows. No consumer reads it; it restates
  each learning file's `sources:` frontmatter in reverse.
- BREAKING for ledger writers: the append path is now shard-addressed, so a writer may no
  longer hardcode `learnings/.ledger.jsonl`. The `.dotfiles` workflow change is a
  coordinated sibling to this one.

## Impact

- Affected specs: `capture-stewardship`
- Affected code: `crates/vaultr/src/validate.rs` (`ledger_path`, ledger validation walk),
  `crates/plant/src/sweep.rs` (`ledger_latest`, `stuck_captures`, `ready_to_seal`), new
  `crates/vaultr/src/ledger.rs`
- Coordinated sibling: `.dotfiles` — `skills/Vault/Workflows/Learn.md` and its append
  helper own the write path and the append-only gate, neither of which lives in this repo
