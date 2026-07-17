# Reconcile Divergent Seals

## Why

A resumed session whose capture was already sealed silently forks the capture
into two epochs. `finish_capture` opens `turns.jsonl` with
`.create(true).append(true)`, so post-resume envelopes land in a fresh raw file
next to the committed `turns.jsonl.zst` that holds the pre-resume turns.
`compress_sweep` never checks for an existing seal, so its `zstd --rm` (no
`-f`) collides and fails every 30-minute tick forever — the launchd log has 73
`seal skipped (… already exists; not overwritten)` lines. Neither file alone is
complete, the raw half is gitignored and unprotected, and the post-resume turns
never reach Learn (both ledgers already contain the sid, and stuck detection
mislabels the dir `seal-blocked`).

Live evidence in today's vault: 26 double-file capture dirs; 25 divergent — 14
where raw < seal (pure post-resume epoch, e.g. `2026/07/15/019f6360-…` raw=1
line vs zst=301), 11 where raw > seal (e.g. `2026/07/13/2eaaca85-…` raw=257 vs
zst=58), one empty seal (`2026/07/16/7edd8ed8-…` zst=0 lines, raw=126). 19 of
the 26 dirs have the same fork on the `herdr.jsonl` sidecar.

## What Changes

- **Reopen on resume**: before appending to a session dir whose
  `turns.jsonl.zst` exists without its raw counterpart, unseal it back to raw
  (same for `herdr.jsonl.zst`) so the session continues as one epoch and the
  normal sweep re-seals the union later. Unseal failure falls back to today's
  fresh-epoch write — capture uptime wins; the doctor pass repairs it later.
- **Seal reconciliation (doctor)**: a pass in the 30-minute compress tick,
  before sealing, merges each double-file dir back to a single raw: identical
  content → drop the raw duplicate; one side a line-prefix of the other → keep
  the superset; otherwise concatenate seal-content then raw (chronological
  epochs). Atomic temp+rename, verify the merged file covers both parts, and
  only then remove the stale seal (its bytes remain in git history from the
  seal commit). Sidecars get the same rule. The existing scrub+seal+commit path
  then re-seals on a later tick.
- **Seal guard**: `compress_sweep` skips dirs whose seal still exists instead
  of retrying a doomed `zstd` call every tick.
- **Visibility**: stuck detection classifies double-file dirs as `divergent`
  (actionable), reported by the watchdog job and `plant sessions stuck`.

## Impact

- Affected specs: capture-stewardship
- Affected code: `crates/plant/src/capture.rs`, `crates/plant/src/sweep.rs`,
  `crates/plant/src/jobs.rs`, `crates/plant/src/main.rs`
