## 1. Block reasons

- [x] 1.1 Split `ReceiptLookup::Absent` and `Pending` out of the shared `Ok(_)` arm in `crates/plant/src/jobs.rs` `reconcile_fence_at` — both still block, but they are different facts
- [x] 1.2 Thread the job name into `reconcile_fence_at` so every block reason can name `plant jobs unblock <name>`
- [x] 1.3 Leave reconciliation semantics untouched: no fence may clear that did not clear before
- [x] 1.4 Unit-test: absent still blocks and names the remedy; pending still blocks and reads differently; a conclusive receipt still appends its record and clears

## 2. Operator unblock

- [x] 2.1 Add `unblock_job(name)` acquiring the same attempt flock a dispatch takes, so it cannot race a live tick
- [x] 2.2 Return success and change nothing when the job holds no fence
- [x] 2.3 Refuse to force a fence `reconcile_fence` would already clear, reporting that the next tick resolves it and appending no record
- [x] 2.4 Otherwise append one durable `failed` ledger record naming the abandoned attempt ID, then remove the fence and `sync_dir`
- [x] 2.5 Wire `jobs unblock <name>` through `crates/plant/src/cli.rs` and `main.rs`, including the usage string
- [x] 2.6 Unit-test: no fence is a no-op; a self-resolving fence is refused unforced; an abandoned fence is cleared leaving exactly one `failed` record

## 3. Proof

- [x] 3.1 Reproduce the incident shape in a fixture — nonretryable fence, no ledger record, no receipt — and assert it still blocks rather than silently re-dispatching
- [x] 3.2 Keep `scheduled_record_failure_blocks_the_next_dispatch` green: a plain script job whose final record failed MUST NOT run twice. This test is the reason auto-clearing on absent was withdrawn
- [x] 3.3 Prove `unblock` clears that fence and leaves exactly one `failed` ledger record `health.15m.sh` would classify
- [x] 3.4 Prove `unblock` refuses a fence backed by a conclusive receipt and writes nothing
- [x] 3.5 `cargo test --workspace`
- [x] 3.6 Deploy via home-manager and drive `plant jobs unblock` against a hand-made fence on the installed binary — `cargo test` passing is not proof the running scheduler behaves
