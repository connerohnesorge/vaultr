# Tasks

## 1. Dropped-turn accounting

- [ ] 1.1 Add dropped-turn count and drop boundary fields to the `Meta` struct in the `vaultr` crate.
- [ ] 1.2 Add a capture function that records one drop into `.meta/<session-id>.json`.
- [ ] 1.3 Add a process-lifetime dropped-turn counter for the case where the `.meta` write fails.
- [ ] 1.4 Call the drop recorder from the `prepare_capture` failure path in `crates/plant/src/proxy.rs`.
- [ ] 1.5 Call the drop recorder from the `finish_capture` failure path in `crates/plant/src/proxy.rs`.
- [ ] 1.6 Call the drop recorder from the WebSocket capture failure path.
- [ ] 1.7 Report the process dropped-turn counter on the health endpoint body.
- [ ] 1.8 Add unit tests for drop recording against a read-only session directory.

## 2. Storage headroom preflight

- [ ] 2.1 Add a free-space query for the vault volume in `crates/plant/src/fsutil.rs`.
- [ ] 2.2 Add a configurable headroom floor with a 2 GiB default.
- [ ] 2.3 Skip the journal write in `prepare_capture` when free space is below the floor.
- [ ] 2.4 Record a drop when the preflight skips the journal write.
- [ ] 2.5 Proceed with the reservation when the free-space query fails.
- [ ] 2.6 Add unit tests for the preflight decision at and below the floor.

## 3. Operator visibility

- [ ] 3.1 Add a low-headroom alert to the health job.
- [ ] 3.2 Add a dropped-turn alert to the health job for each affected session.
- [ ] 3.3 Report the recorded drop count in the `plant sessions coverage` output.
- [ ] 3.4 Mark a capture with recorded drops as known-incomplete in the coverage report.
- [ ] 3.5 Add tests for both new health alerts.

## 4. Validation

- [ ] 4.1 Run `cargo test --workspace` and resolve every failure.
- [ ] 4.2 Run `plant --self-test` and confirm the capture path still passes.
- [ ] 4.3 Simulate a full volume against a temporary vault and confirm the drop is recorded.
