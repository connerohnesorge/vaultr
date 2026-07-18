## 1. Deepen the Herdr module

- [x] 1.1 Move the concrete agent-run request, cleanup policy, outcome, typed Herdr response parsing, workspace recovery, prompt delivery, completion wait, and focus-restoring cleanup into `plant::herdr` behind one high-level operation.
- [x] 1.2 Reduce `jobs.rs` to job definitions, eligibility, launch and prompt construction, cadence, recording, and delegation to the Herdr lifecycle interface while preserving current outcomes.

## 2. Verify the interface

- [x] 2.1 Add runnable tests for typed pane and workspace decoding plus cleanup and outcome decisions without adding a fake command adapter or explicit state machine.
- [x] 2.2 Run `cargo test --workspace` and a real live-Herdr smoke check that verifies unfocused workspace creation, verified prompt delivery, completion status, pane cleanup or retention, and user-focus preservation.
