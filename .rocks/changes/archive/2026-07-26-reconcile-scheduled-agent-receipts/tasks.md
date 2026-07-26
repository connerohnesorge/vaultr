# Tasks

## 1. Attempt ID export

- [ ] 1.1 Thread the published attempt ID into scheduled and manual script execution.
- [ ] 1.2 Set `PLANT_ATTEMPT_ID` in the job script environment.

## 2. Receipt reconciliation

- [ ] 2.1 Add a read-only keyed Agent Run receipt lookup in `crates/plant/src/agent_run.rs`.
- [ ] 2.2 Append a durable final ledger record for a conclusive receipt during fence reconciliation.
- [ ] 2.3 Retain the fence for an absent, pending, unreadable, or mismatched receipt.

## 3. Verification

- [ ] 3.1 Add a regression for durable receipt lookup after the scheduler stops.
- [ ] 3.2 Add a regression for succeeded receipt reconciliation without another Herdr launch.
- [ ] 3.3 Add a regression for failed receipt reconciliation without another Herdr launch.
- [ ] 3.4 Add a regression that retains fences for nonconclusive receipts.
- [ ] 3.5 Run the Plant test suite.
