# Change: Deepen the Herdr agent lifecycle module

## Why

Plant's scheduler currently owns Herdr schema parsing, workspace recovery, focus restoration, readiness checks, prompt delivery, completion waits, and cleanup in addition to job policy. These ordering invariants need one small interface so changes and failures stay local.

## What Changes

- Move concrete Herdr lifecycle mechanics from `jobs.rs` into the existing `herdr.rs` module.
- Expose one high-level agent-run interface with explicit execution inputs, cleanup policy, and outcome.
- Keep scheduling, eligibility, launch construction, prompts, cadence, and outcome recording in `jobs.rs`.
- Preserve current failure, retry, focus, workspace-reclamation, and pane-retention behavior.
- Add focused pure tests plus one real-Herdr lifecycle smoke check without a generic adapter trait or fake command runner.

## Impact

- Affected specs: `plant-agent-jobs`
- Affected code: `crates/plant/src/jobs.rs`, `crates/plant/src/herdr.rs`, focused Plant tests
- Unchanged: Herdr plugin scripts, job cadence, agent models and flags, session capture snapshots
