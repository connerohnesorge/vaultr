# Reconcile scheduled attempts through keyed Agent Run receipts

## Problem

A Plant restart can strand a completed scheduled job behind a permanent fence.

The `context-audit` job proves the gap. Plant published attempt
`4ff366d1-fb27-4c3d-86e9-27cfc9929abf` at 2026-07-25T18:09:50Z. The Herdr pane
reached `done` at 2026-07-25T19:24:51Z. The audit filed its Rocks proposal. Plant
restarted before the scheduler recorded a final ledger record. Every later
scheduler tick reads the nonretryable fence and refuses to dispatch.

Fence reconciliation reads only the job ledger. It cannot see that the Herdr
effect completed. The job has been silent since 2026-07-25T15:09:04Z.

## Root cause

Scheduled attempts and keyed Agent Runs hold separate durable state.

`reconcile_fence_at` in `crates/plant/src/jobs.rs` clears a nonretryable fence
only for a matching ledger record. The keyed Agent Run receipt in
`crates/plant/src/agent_run.rs` already records a conclusive outcome durably.
Nothing connects the two records.

The job scripts also cannot supply the scheduled attempt ID as an idempotency
key. Plant does not export that ID to the script environment.

## Fix

Connect the two existing records. Add no new recovery journal.

Plant exports the published attempt ID as `PLANT_ATTEMPT_ID`. An agent-backed job
script passes that value to `plant agent run --idempotency-key`. The Agent Run
child claims the key before the Herdr side effect. The child persists its
conclusive receipt before it exits.

Fence reconciliation reads that receipt only when the ledger holds no matching
record. A conclusive receipt becomes one durable final ledger record. Plant
clears the fence only after that record is durable.

An absent, pending, unreadable, or mismatched receipt keeps the current
fail-closed behavior.

## Rejected alternatives

Clearing fences by age can duplicate an uncertain Herdr effect. A script-owned
completion marker would duplicate the existing receipt protocol.
