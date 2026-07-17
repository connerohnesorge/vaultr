# Tasks

## 1. Hygiene Cultivation Job

- [ ] 1.1 Add `Kind::Hygiene` and an hourly pure-Rust `hygiene` job to `load_jobs()`, updating the module doc comment and the jobs-shape test (7 jobs → 8)
- [ ] 1.2 Implement the hygiene sweep with the existing async git helpers: push-when-ahead, grace-windowed commit+push scoped to `learnings/ preferences/ digests/`, detect-only report for out-of-scope dirt and a pending dotfiles `vault` submodule bump
- [ ] 1.3 Unit tests: job shape, grace-window boundary, staging-scope guard (never stages outside the Learn-owned trio), outcome mapping for push failure and detect-only findings
- [ ] 1.4 Run `cargo test --workspace` green and confirm `plant --self-test` is unaffected
