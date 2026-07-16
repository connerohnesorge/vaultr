# Add Plant Reflect Job

## Why

Learn passes extract per-session atomic learnings; reconcile removes
contradictions and staleness. Nothing synthesizes *across* the accumulated
learnings — recurring themes never get promoted into preferences, runbooks, or
Rocks proposals. A scheduled cross-session reflection closes that gap.

## What Changes

- New `Kind::Reflect` Cultivation Job in plant: daily, Claude `opus[1m]`,
  dispatches `/Vault reflect` into a Herdr pane (same lifecycle as learn).
- Skips when no new learnings landed since the last reflect attempt
  (learnings ledger mtime vs the job's last record ts).
- The `/Vault reflect` workflow itself lives in the dotfiles Vault skill
  (out of scope for this repo).

## Impact

- Affected specs: plant-agent-jobs
- Affected code: `crates/plant/src/jobs.rs`
