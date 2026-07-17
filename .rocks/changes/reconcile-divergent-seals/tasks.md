# Tasks — Reconcile Divergent Seals

## 1. Reopen sealed captures on resume

- [ ] 1.1 Unseal `turns.jsonl.zst` (and `herdr.jsonl.zst`) back to raw before the first append into a sealed session dir in `capture.rs`, non-fatal fallback to the fresh-epoch write, with unit tests covering the reopen and the fallback paths

## 2. Reconcile divergent seals in the compress tick

- [ ] 2.1 Add a reconciliation pass to `sweep.rs` that runs before sealing: identical / prefix / concat merge rules into the raw file via atomic temp+rename, verify the merge covers both parts, then remove the stale seal — applied to `turns.jsonl` and the `herdr.jsonl` sidecar, with unit tests for all five spec scenarios including the empty seal

- [ ] 2.2 Guard sealing in `compress_sweep` to skip dirs whose seal still exists (ending the per-tick `zstd: already exists` retry loop), with a test

## 3. Surface divergence

- [ ] 3.1 Classify double-file dirs as `divergent` in `stuck_captures` (checked before ledger states) and count it as actionable in `watchdog_summary`, with tests updated for the new state

## 4. Prove on the live vault

- [ ] 4.1 `cargo test --workspace` and `plant --self-test` green

- [ ] 4.2 Rebuild, run one reconcile+seal cycle against the real vault, and verify: the 26 recorded double-file dirs (e.g. `2026/07/13/2eaaca85-…`, `2026/07/16/7edd8ed8-…`, `2026/07/17/8d587b7e-…`) each collapse to a single seal whose line count covers both prior parts, a seal commit lands in the vault repo, and `plant sessions stuck` reports zero divergent captures
