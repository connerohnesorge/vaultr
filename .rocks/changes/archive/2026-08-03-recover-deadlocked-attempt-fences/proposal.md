# Change: Recover deadlocked scheduled-job attempt fences

## Why

Three Plant jobs stopped running and nothing noticed for days. `learn-codex` last ran
2026-07-24T16:23Z, `reflect` and `reconcile` 2026-07-26T00:45Z and 00:48Z. Between them
they refused 15,019 scheduled dispatches — 7,003, 4,007 and 4,009 — each logging
`scheduled dispatch blocked: attempt <id> has no durable final outcome` and then doing
nothing. Recovery required deleting state by hand.

Each held a nonretryable fence whose Agent Run receipt was absent: hashing each attempt
ID to its receipt path under `agent-runs/` found no file. Blocking there is correct —
see ADR-0001 — but nothing can ever change it. No process will write a receipt for an
attempt that is no longer running, and no ledger record will appear for an attempt that
never finished. The state is terminal and the scheduler has no exit from it.

Discovery failed too. A blocked dispatch appends no ledger record, and `health.15m.sh`
classifies jobs from their ledgers, so the alert read only `learn-codex silent: no run
since 2026-07-24T13:52Z (cadence 15m)` across 382 sweeps — naming no cause and no
remedy. A job that is blocked and a job that is idle are indistinguishable from the
alert, which is why this ran for five days.

## What Changes

- Add `plant jobs unblock <name>`: the operator exit for a fence no reconciliation can
  resolve. It appends a `failed` ledger record naming the abandoned attempt, then clears
  the fence — so the abandonment reaches `health.15m.sh` instead of staying silent.
- Refuse to force a fence that reconciliation would already clear, so the command can
  never pre-empt a conclusive receipt or a matching ledger record.
- Take the same attempt lock a dispatch takes, so the command cannot race a live tick.
- Name the remedy in every block reason, so the log line says what to run.
- Distinguish the pending block reason from the absent one. Both still block; they are
  different facts and a five-day outage is not the moment to discover they read alike.

## Impact

- Affected specs: `plant-agent-jobs`
- Affected code: `crates/plant/src/jobs.rs` (`reconcile_fence_at` block reasons, new
  `unblock_job`); `crates/plant/src/cli.rs`; `crates/plant/src/main.rs`
- No `.dotfiles` sibling. `health.15m.sh` already alerts on a `failed` ledger record.
- Reconciliation semantics are unchanged: no fence clears itself that did not before.
