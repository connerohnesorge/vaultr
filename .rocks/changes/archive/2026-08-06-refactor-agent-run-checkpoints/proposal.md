# Change: Refactor Agent Run checkpoints

## Why

`AgentRunIdentity` combines lifecycle phase, progressively discovered identity,
and capture correlation in one product type. The type permits phase and field
combinations that recovery cannot interpret.

## What Changes

- Replace the flat identity tuple with tagged phase-specific checkpoints.
- Keep workspace and pane identity stable across checkpoint updates.
- Represent terminal-only and session-bound pane identity explicitly.
- Preserve safe handling for existing pending receipt formats.
- Add tests for checkpoint transitions, persistence ordering, and recovery.

## Impact

- Affected specs: `plant-agent-jobs`
- Affected code: `crates/plant/src/herdr.rs`, `crates/plant/src/agent_run.rs`
- Affected state: keyed Agent Run in-progress receipts
