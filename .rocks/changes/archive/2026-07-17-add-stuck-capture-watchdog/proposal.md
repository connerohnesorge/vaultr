# Add Stuck-Capture Watchdog

## Why

The cultivation pipeline fails silently. A raw Session Capture stays unsealed
forever when compress keeps failing on it after both learners ledgered it, when
one learner never processes it (Sealing requires both), when learn never picks
up a substantive capture, or when it sits below learn's substance gate (never
learnable, therefore never sealable). Nothing detects any of these states —
silence is indistinguishable from health. Live evidence in today's vault:
53 raw captures idle >24h; at >48h idle that is 5 seal-blocked, 1
half-learned (codex missing), and 20 sub-threshold.

## What Changes

- New read-only sweep primitive in `sweep.rs` that classifies every raw
  Session Capture idle beyond an age threshold by learn-ledger state:
  `seal-blocked`, `half-learned:<missing learner>`, `unlearned`,
  `sub-threshold`.
- New in-process `watchdog` Cultivation Job (6h cadence, no agent pane): runs
  the sweep, logs one line per stuck capture, records `success`/`failed` with
  per-state counts. Sub-threshold is informational only and never fails the
  job.
- New `plant sessions stuck [--age 24h]` subcommand for manual inspection
  (exit 0 healthy / 1 actionable findings).
- Detect-only: no remediation, no writes to Session Captures.

## Impact

- Affected specs: capture-stewardship (new capability)
- Affected code: `crates/plant/src/sweep.rs`, `crates/plant/src/jobs.rs`,
  `crates/plant/src/main.rs`
