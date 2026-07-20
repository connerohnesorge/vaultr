# Tasks: Observation-window capture coverage audit

## 1. Coverage computation

- [x] 1.1 Expose Reconstruction's canonical sealed-then-live-raw traversal as a closure-based streaming Envelope seam and use it from `coverage(vault, sid)`
- [x] 1.2 Resolve window start as earliest captured `observed_at`, falling back to meta `original_start`; read the transcript path from meta
- [x] 1.3 Stream the native transcript and classify distinct `requestId`s against the window start (pre-window → carryover, in-window → denominator)
- [x] 1.4 Derive harness support from Envelope truth and fail for Codex or a zero in-window native-ID denominator

## 2. CLI surface

- [x] 2.1 Wire `plant sessions coverage <sid>` in `main.rs` to print coverage, carryover count, and residual missing ids; exit non-zero on a genuine gap, unsupported harness, empty denominator, or evidence error

## 3. Validation

- [x] 3.1 Unit test: resumed capture with pre-window transcript history reports carryover, not loss
- [x] 3.2 Unit test: complete multi-call Claude window reports 100% and empty residual
- [x] 3.3 Unit test: one in-window native id absent from Envelopes appears in residual and lowers coverage
- [x] 3.4 Unit test: mixed sealed/raw generations entered through either sibling include concatenated records, tolerate only the live tail, and report the two fixture gaps
- [x] 3.5 Unit test malformed/unreadable evidence and large-record correctness; prove bounded release-CLI RSS with constant ID cardinality at 128 MiB and at least 919 MiB decoded capture evidence
- [x] 3.6 CLI test: Codex, empty-native, and all-carryover Claude denominators fail explicitly without a percentage
- [x] 3.7 Run the existing release Plant self-test in CI so persisted completion, telemetry, and Codex trailing-output wiring stay covered
