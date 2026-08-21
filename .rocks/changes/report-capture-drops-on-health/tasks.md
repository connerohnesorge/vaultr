# Tasks

## 1. Drop counter

- [ ] 1.1 Add a process-lifetime recorded-drop counter beside `UNRECORDED_DROPS` in `crates/plant/src/capture.rs`.
- [ ] 1.2 Increment the recorded-drop counter when `record_drop` writes the marker.
- [ ] 1.3 Add a public reader for the recorded-drop counter.

## 2. Health body

- [ ] 2.1 Add `recorded_drops` to `health_body` in `crates/plant/src/proxy.rs`.
- [ ] 2.2 Add `headroom_bytes` to `health_body` from `free_bytes` on the vault volume.
- [ ] 2.3 Report `headroom_bytes` as null when the volume cannot be measured.
- [ ] 2.4 Add `headroom_floor` to `health_body` from `headroom_floor`.
- [ ] 2.5 Add `capture_ok` to `health_body`.
- [ ] 2.6 Set `capture_ok` to false when either drop counter is above zero.
- [ ] 2.7 Set `capture_ok` to false when measured headroom is below the floor.
- [ ] 2.8 Keep `ok` bound to process liveness.

## 3. Validation

- [ ] 3.1 Add a test that asserts `capture_ok` is true on a healthy process.
- [ ] 3.2 Add a test that asserts `capture_ok` is false after a recorded drop.
- [ ] 3.3 Add a test that asserts `recorded_drops` counts a recorded drop.
- [ ] 3.4 Add a test that asserts `capture_ok` is false when headroom is below the floor.
- [ ] 3.5 Add a test that asserts an unmeasurable volume reports a null `headroom_bytes`.
- [ ] 3.6 Add a test that asserts an unmeasurable volume keeps `capture_ok` true.
- [ ] 3.7 Run `cargo test -p plant`.
