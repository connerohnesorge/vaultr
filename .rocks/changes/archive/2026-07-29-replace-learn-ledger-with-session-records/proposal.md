# Change: Replace the learn ledger with immutable per-pass session records

## Why

Learn state lives in `vault/learnings/.ledger.jsonl`, one shared mutable file appended
by learn agents. Two learners already destroyed each other's rows on this machine:
Codex commit `9510bfc` appended ten rows, and thirty-six seconds later Claude commit
`1bca018` rewrote the file from a stale snapshot, deleting all ten. Plant rescheduled
those sessions and their captures became unsealable.

Learn is moving onto the nix allocator fleet, which turns that single-machine hazard
into a sync-wedging one: the existing commit gate diffs staged content against local
HEAD, so it cannot see another host's rows. Each host's gate passes, the push goes
non-fast-forward, and `commit-push.30m.sh` falls to `git merge` with no `union` driver
in `vault/.gitattributes`. That job also pushes Session Captures and the Session Index,
so one ledger collision stalls all vault sync.

Every one of those failures comes from learn state being an aggregate file. It does not
need to be one. Each session already owns a directory that Plant's maintenance sweep
walks in full on every pass, and 1,772 of the 1,775 sessions with learn state have one.
Storing each pass as its own immutable file inside that directory removes the shared
mutable file, and with it the entire failure class — no merge conflict, no lost update,
no stale-snapshot rewrite, no coordination between writers at all.

Learn state cannot simply be dropped in favour of Git or frontmatter. See ADR-0001.

## What Changes

- Record each completed learn pass as its own immutable file in that session's own
  directory, named for the learner, host, and pass timestamp.
- Derive the learner, writing host, and identity of a record from its path, so a record
  cannot disagree with its own location and no membership validation is needed.
- Never modify or replace a learn record once written. A resumed session simply records
  another pass, needing no exception.
- Read learn state through one reader that folds those records together with the frozen
  legacy ledger, latest pass winning per learner.
- Fold learn records during the session-directory walk the maintenance sweep already
  performs, rather than as a separate traversal.
- Freeze `learnings/.ledger.jsonl` as read-only history. Its rows keep counting; nothing
  migrates them.
- BREAKING for ledger writers: appending to the legacy ledger no longer records a pass.
  The `.dotfiles` sibling change owns the write path.

## Impact

- Affected specs: `capture-stewardship`
- Affected code: new `crates/vaultr/src/learn.rs`; `crates/vaultr/src/validate.rs`
  (`ledger_path`, ledger validation walk); `crates/plant/src/sweep.rs` (`ledger_latest`,
  `session_generations`, `stuck_captures`, `ready_to_seal`, `eligible_candidates`)
- Coordinated sibling: `.dotfiles` — the append helper, `Learn.md`, and the `verify` and
  `reflect` jobs that read learn state all live there. This change MUST land first so
  records are readable before any writer moves.
