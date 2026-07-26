# Plant Agent Jobs Design

## ADRs

### ADR-0001: Herdr owns the complete agent-run lifecycle

Job code owns selection, prompt construction, cadence, retention policy, and
outcome recording. Herdr owns one complete effectful run: availability,
workspace creation, native agent readiness, one acknowledged pane-scoped
status subscription, one checked prompt submission, observation of this run's
`working`-to-`done` transition, terminal/session identity revalidation, and
cleanup. This keeps raw Herdr mechanics out of the scheduler and prevents a
pre-existing `done`, missed fast turn, failed submission, or reused pane from
being accepted as the current run.

Plant's agent-run boundary surrounds that lifecycle with durable keyed receipt
state before side effects and durable conclusive outcomes before return.
Unkeyed callers retain the legacy human-output contract. Consumers MUST NOT
reimplement either lifecycle; Vault Doors' typed client is described by
`vault-doors/design.md` ADR-0003.

### ADR-0002: Scheduled attempts fence only admitted execution

A scheduled dispatch waits for process-local scheduler capacity without a durable attempt fence.
Cancellation during capacity waiting therefore leaves no uncertain execution record.

After admission, dispatch acquires the per-job attempt guard.
The guard covers ledger verification, reconciliation, cadence recheck, fence publication, the scheduled action, and one durable final or retryable transition.
If the guard or its state cannot be loaded or published safely, dispatch fails closed.
Competing processes may each obtain local capacity, but the per-job flock serializes their durable cadence checks.

The process deadline covers the direct child and both captured output drains,
so a descendant retaining an output pipe cannot strand the guard. Capture work
uses the same attempt fence, but listener retention and capture-descendant
draining remain owned by `capture-stewardship/design.md` ADR-0001.

### ADR-0003: The attempt ID is the Agent Run idempotency key

Plant exports the published attempt ID to every job script as `PLANT_ATTEMPT_ID`.
An agent-backed script passes that value to `plant agent run --idempotency-key`.
The two durable records therefore share one identity.

Fence reconciliation reads the keyed receipt only when the job ledger holds no
matching record. A conclusive receipt becomes one durable final ledger record
before the fence clears. This recovers a run that finished while its scheduler
died, without a second Herdr lifecycle and without a second recovery journal.

An absent, pending, unreadable, or mismatched receipt keeps the existing
fail-closed behavior. Reconciliation never treats an uncertain receipt as proof.
