# Add Plant Hygiene Job

## Why

Vault git health is accidental today: pushes only happen as a piggyback on the
compress job's sealing path, and when they fail they are print-only — the two
most recent seal pushes both logged `push FAILED (next sweep retries)` with no
job record and no retry until another seal happens to run. Learn commits sit
unpushed on quiet capture days, a dead learn agent can strand uncommitted
learnings, and thousands of uncommitted Build-output files plus a pending
dotfiles submodule bump accumulate with nothing watching.

## What Changes

- New `Kind::Hygiene` Cultivation Job in plant: hourly, pure in-process Rust
  (no agent, no Herdr pane), reusing the sweep's async git helpers.
- Deterministic remediation, narrow scope:
  - Push the vault repo whenever it is ahead of upstream — decoupling push from
    sealing; a failed push records a `failed` outcome instead of a log line.
  - Commit stray Learn-owned output (`learnings/`, `preferences/`, `digests/`)
    only, and only when the newest dirty file is older than a 30-minute grace
    window (never races a live learn pane), then push.
- Detect-and-report only (never remediates):
  - Uncommitted vault paths outside the Learn-owned trio and `sessions/`
    (Build/reconcile output — ownership is ambiguous, a human decides).
  - A pending dotfiles `vault` submodule bump (committing in `~/.dotfiles` is
    Conner's, by contract).
- Hard boundaries: never stages outside the Learn-owned trio, never commits or
  pushes in the dotfiles repo, never force-pushes, never pulls or rebases — a
  non-fast-forward push records `failed` and stops.

## Impact

- Affected specs: plant-agent-jobs
- Affected code: `crates/plant/src/jobs.rs`, `crates/plant/src/sweep.rs`
