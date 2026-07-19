# Tasks — Add Door Jobs

## 1. Polyglot job execution in Plant

- [ ] 1.1 Make the job filename parse extension-agnostic (`<name>.<interval>.<ext>`) in `crates/plant/src/jobs.rs` with unit tests covering `.ts`, `.sh`, and skipped non-interval files
- [ ] 1.2 Exec job paths directly (shebang-selected interpreter) instead of `/bin/bash`, recording ENOEXEC/spawn failures in the job ledger, with a test for the failure path
- [ ] 1.3 Sweep existing `vault/jobs/*.sh` for missing shebangs and update `vault/jobs/AGENTS.md` to require executable + shebang

## 2. Door library (Bun/TypeScript)

- [ ] 2.1 Scaffold the `ts/` Bun package in the workspace with flake + Home Manager packaging so door jobs can import it
- [ ] 2.2 Implement the typed `plant agent run` wrapper surfacing `Unavailable`/`Failed`/`Succeeded`, with a test against a stubbed binary
- [ ] 2.3 Implement `door()`: glob scan vs persisted high-water mark, filter predicate, batch prompt build, fence advance only after outcome recording, with unit tests for double-fire and Unavailable-no-advance
- [ ] 2.4 Implement the ingestion-root allowlist (loud rejection before launch) and the rolling-window breaker with manual re-arm, with tests for both

## 3. First doors and ingestion

- [ ] 3.1 Add a Teamer sync job to `vault/jobs/` landing Teams chats as watchable Vault Content on a 30m cadence
- [ ] 3.2 Add one real email door and one real Teams door as `door-<name>.30m.ts` jobs and prove an end-to-end fire against a live Herdr session (message file → agent session → ledger outcome)
