# Tasks: Observation-window capture coverage audit

## 1. Coverage computation

- [ ] 1.1 Add `coverage(vault, sid)` in `sweep.rs` returning window start, in-window native count, captured count, out-of-scope carryover count, and residual missing `request-id`s (read-only over Envelopes + transcript)
- [ ] 1.2 Resolve window start as earliest captured `observed_at`, falling back to meta `original_start`; read the transcript path from meta
- [ ] 1.3 Classify native `requestId`s by first-seen transcript timestamp against the window start (pre-window → carryover, in-window → denominator)

## 2. CLI surface

- [ ] 2.1 Wire `plant sessions coverage <sid>` in `main.rs` to print coverage, carryover count, and residual missing ids; exit non-zero only on genuine in-window gap

## 3. Validation

- [ ] 3.1 Unit test: resumed capture with pre-window transcript history reports carryover, not loss
- [ ] 3.2 Unit test: fully-captured window reports 100% and empty residual
- [ ] 3.3 Unit test: one in-window native id absent from Envelopes appears in residual and lowers coverage
