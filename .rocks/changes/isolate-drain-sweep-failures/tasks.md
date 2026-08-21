# Tasks

## 1. Make stage cleanup idempotent

- [ ] 1.1 Treat a not-found error from the stage removal in `commit_stage` as success.
- [ ] 1.2 Keep every other stage removal error fatal.
- [ ] 1.3 Add a test that removes an already absent stage without failing.

## 2. Widen retired stage reconciliation

- [ ] 2.1 Accept a retired stage at any sequence below the drain head in `commit_stage`.
- [ ] 2.2 Require the retired Envelope to be present in the raw generation.
- [ ] 2.3 Add a test covering orphan stages many sequences below the drain head.

## 3. Isolate per-session sweep failures

- [ ] 3.1 Split the recovery failure policy into a fail-fast mode and an isolating mode.
- [ ] 3.2 Keep `recover_all` on the fail-fast mode for startup.
- [ ] 3.3 Move `recover_live` to the isolating mode.
- [ ] 3.4 Collect each per-session failure and return one aggregate sweep error.
- [ ] 3.5 Keep an inventory build failure immediately fatal in both modes.
- [ ] 3.6 Add a test proving one damaged Session Capture does not block the others.

## 4. Quarantine irreconcilable evidence

- [ ] 4.1 Add a quarantine directory beside the capture staging root.
- [ ] 4.2 Move an irreconcilable stage into quarantine during the isolating mode only.
- [ ] 4.3 Preserve the quarantined stage bytes without modification.
- [ ] 4.4 Record one dropped turn per quarantined stage with its reason and sequence.
- [ ] 4.5 Continue draining the remaining sequences after a quarantine.
- [ ] 4.6 Add a test proving startup recovery still fails on conflicting evidence.

## 5. Account for stranded backlog

- [ ] 5.1 Count undrained staged Envelopes for each Session Capture.
- [ ] 5.2 Report the total stranded backlog on both health endpoints.
- [ ] 5.3 Record one dropped turn per undrained Envelope at generation Sealing.
- [ ] 5.4 Add a test asserting a drained Session Capture reports a zero backlog.

## 6. Prove the fix against the damaged host state

- [ ] 6.1 Reproduce the 24 orphan stages of session `b29c4c65-2e14-4a00-90bd-8e056ada249d` in a fixture.
- [ ] 6.2 Assert the sweep clears that fixture backlog in one pass.
- [ ] 6.3 Assert `plant sessions coverage` reports complete in-window coverage for a fixture with a reaped head.
