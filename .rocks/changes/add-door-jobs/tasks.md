# Tasks — Add Door Jobs

## 1. Polyglot job execution in Plant

- [ ] 1.1 Make the job filename parse extension-agnostic (`<name>.<interval>.<ext>`) in `crates/plant/src/jobs.rs` with unit tests covering `.ts`, `.sh`, and skipped non-interval files
- [ ] 1.2 Exec job paths directly (shebang-selected interpreter) instead of `/bin/bash`, recording ENOEXEC/spawn failures in the job ledger, with a test for the failure path
- [ ] 1.3 Sweep existing `vault/jobs/*.sh` for missing shebangs and update `vault/jobs/AGENTS.md` to require executable + shebang

## 2. Door library (Bun/TypeScript)

- [ ] 2.1 Scaffold the `ts/` Bun package in the workspace with flake + Home Manager packaging so door jobs can import it
- [x] 2.2 Deepen `plant agent run` with a stable idempotency key, durable fail-closed claim/outcome lookup, and machine-readable separation of durable, retryable, and indeterminate results, with focused Rust tests
- [x] 2.3 Implement `door()`: fail-closed atomic state, per-door cross-process lock, timestamp frontier with durable tied paths, exact persisted batch claim, stable claim-derived idempotency key, and durable-outcome-only frontier advance
- [x] 2.4 Implement one canonical ingestion-root resolver (loud traversal and symlink-escape rejection before launch) and the rolling-window breaker with manual re-arm, with tests for both
- [x] 2.5 Add corrupt-state, tied-mtime, concurrent-process, before-launch crash, and after-launch crash tests against a fake idempotent `plant agent run`
- [x] 2.6 Replace the cursor with a timestamp frontier and durable seen paths, require machine-readable durable Plant outcomes before claim advance, canonicalize every path beneath one selected ingestion root, and cover traversal, symlink, indeterminate-result, and lower-sorting tied-mtime cases

## 3. First doors and ingestion

- [ ] 3.1 Add a Teamer sync job to `vault/jobs/` landing Teams chats as watchable Vault Content on a 30m cadence
- [ ] 3.2 Add one real email door and one real Teams door as `door-<name>.30m.ts` jobs and prove an end-to-end fire against a live Herdr session (message file → agent session → ledger outcome)
