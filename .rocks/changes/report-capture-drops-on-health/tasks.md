# Tasks

## 1. Drop counter

- [x] 1.1 Add a process-lifetime recorded-drop counter beside `UNRECORDED_DROPS` in `crates/plant/src/capture.rs`.
- [x] 1.2 Increment the recorded-drop counter when `record_drop` writes the marker.
- [x] 1.3 Add a public reader for the recorded-drop counter.

## 2. Health body

- [x] 2.1 Add `recorded_drops` to `health_body` in `crates/plant/src/proxy.rs`.
- [x] 2.2 Add `headroom_bytes` to `health_body` from `free_bytes` on the vault volume.
- [x] 2.3 Report `headroom_bytes` as null when the volume cannot be measured.
- [x] 2.4 Add `headroom_floor` to `health_body` from `headroom_floor`.
- [x] 2.5 Add `capture_ok` to `health_body`.
- [x] 2.6 Set `capture_ok` to false when either drop counter is above zero.
- [x] 2.7 Set `capture_ok` to false when measured headroom is below the floor.
- [x] 2.8 Keep `ok` bound to process liveness.

## 3. Validation

- [x] 3.1 Add a test that asserts `capture_ok` is true on a healthy process.
- [x] 3.2 Add a test that asserts `capture_ok` is false after a recorded drop.
- [x] 3.3 Add a test that asserts `recorded_drops` counts a recorded drop.
- [x] 3.4 Add a test that asserts `capture_ok` is false when headroom is below the floor.
- [x] 3.5 Add a test that asserts an unmeasurable volume reports a null `headroom_bytes`.
- [x] 3.6 Add a test that asserts an unmeasurable volume keeps `capture_ok` true.
- [x] 3.7 Run `cargo test -p plant`.
