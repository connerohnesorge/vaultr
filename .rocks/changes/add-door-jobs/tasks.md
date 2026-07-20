# Tasks — Add Door Jobs

## 1. Polyglot job execution in Plant

- [x] 1.1 Make the job filename parse extension-agnostic (`<name>.<interval>.<ext>`) in `crates/plant/src/jobs.rs` with unit tests covering `.ts`, `.sh`, and skipped non-interval files
- [x] 1.2 Exec job paths directly (shebang-selected interpreter) instead of `/bin/bash`, recording ENOEXEC/spawn failures in the job ledger, with a test for the failure path
- [x] 1.3 Sweep existing `vault/jobs/*.sh` for missing shebangs and update `vault/jobs/AGENTS.md` to require executable + shebang

## 2. Door library (Bun/TypeScript)

- [x] 2.1 Scaffold the `ts/` Bun package in the workspace with flake + Home Manager packaging so door jobs can import it
- [x] 2.2 Deepen `plant agent run` with a stable idempotency key, durable fail-closed claim/outcome lookup, and machine-readable separation of durable, retryable, and indeterminate results, with focused Rust tests
- [x] 2.3 Implement `door()`: fail-closed atomic state, per-door cross-process lock, timestamp frontier with durable tied paths, exact persisted batch claim, stable claim-derived idempotency key, and durable-outcome-only frontier advance
- [x] 2.4 Implement one canonical ingestion-root resolver (loud traversal and symlink-escape rejection before launch) and the rolling-window breaker with manual re-arm, with tests for both
- [x] 2.5 Add corrupt-state, tied-mtime, concurrent-process, before-launch crash, and after-launch crash tests against a fake idempotent `plant agent run`
- [x] 2.6 Replace the cursor with a timestamp frontier and durable seen paths, require machine-readable durable Plant outcomes before claim advance, canonicalize every path beneath one selected ingestion root, and cover traversal, symlink, indeterminate-result, and lower-sorting tied-mtime cases
- [x] 2.7 Durably link new Plant/Door state directories, migrate shipped hwm/v1 states conservatively, publish complete locks atomically, restore Bun.Glob no-follow matching, and replace the receipt fields with one tagged enum
- [x] 2.8 Fail closed on incomplete/invalid legacy state, prove guarded stale takeover, bind claims to stable bounded-content identities from nonblocking no-follow descriptor reads with cross-platform Bun CI, preserve the v1 cursor/key boundary, and separate Plant durable state and Agent Run receipt ownership

## 3. First doors and ingestion

- [x] 3.1 Add a Teamer sync job to `vault/jobs/` landing Teams chats as watchable Vault Content on a 30m cadence
- [x] 3.2 Add one real email door over machine-local immutable Internet-Message-ID artifacts and one real Teams door as `door-<name>.30m.ts` jobs, and prove an isolated end-to-end mail fire against live Herdr (JSONL projection → immutable artifact → one durable agent outcome)
