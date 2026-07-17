# Tasks

## 1. Detection sweep

- [ ] 1.1 Add stuck-capture classification over idle raw captures in `sweep.rs` using per-learner ledger membership and the shared learn substance gate
- [ ] 1.2 Unit tests covering every state plus the age gate and sealed/active exemptions

## 2. Watchdog job and CLI

- [ ] 2.1 Register the in-process `watchdog` job (6h cadence) recording success/failed with per-state counts; sub-threshold never fails it
- [ ] 2.2 Add `plant sessions stuck [--age <duration>]` subcommand (exit 0 healthy / 1 actionable)
- [ ] 2.3 Extend jobs tests: job count/shape and the watchdog outcome policy

## 3. Verification

- [ ] 3.1 `cargo test --workspace` green
- [ ] 3.2 Run the built binary against the real vault and confirm the known stuck captures are reported by `plant sessions stuck` and `plant jobs run watchdog`
