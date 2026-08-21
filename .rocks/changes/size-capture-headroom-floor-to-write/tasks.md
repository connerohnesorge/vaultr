# Tasks

## 1. Headroom floor default

- [x] 1.1 Change the `headroom_floor` default in `crates/plant/src/fsutil.rs` to 64 MiB.
- [x] 1.2 Update the doc comment on `headroom_floor` to state the peak demand of one capture write.

## 2. Validation

- [x] 2.1 Add a test that asserts the default floor is 67108864 bytes.
- [x] 2.2 Add a test that asserts `PLANT_CAPTURE_HEADROOM_BYTES` overrides the default.
- [x] 2.3 Add a capture test that reserves a sequence with 1 GiB of free space reported.
- [x] 2.4 Run `cargo test -p plant`.
