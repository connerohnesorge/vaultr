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

### ADR-0004: Only typed compression fences replay in process

Every new scheduled attempt fence records `Script` or `InProcessCompression`.
Manual job execution records `Script`.
An absent kind identifies a legacy fence.

A matching durable ledger record remains authoritative and clears the fence before replay.
An unresolved `InProcessCompression` fence returns its held attempt guard to the listener-owning daemon.
The daemon reuses the fenced attempt ID without publishing another fence.
Cadence remains secondary until that attempt reaches durable completion.
Restart-independent workers accept only `Script` actions.
The listener-owning daemon retains every `InProcessCompression` action.

This replay relies on `capture-stewardship/design.md` ADR-0001 for crash-recoverable Sealing.
Script and legacy actionless fences remain fail-closed because their side effects are not proven replay-safe.

### ADR-0005: Reconcile keyed Agent Runs by durable execution identity

A pending receipt stores the Herdr workspace, pane, terminal, captured session, and last observed lifecycle stage.
Plant uses that identity to inspect the exact execution after a supervisor restart.
It never treats receipt age as proof that the effect is dead.

A matching live pane resumes observation.
A matching terminal capture produces a conclusive receipt.
A conclusively absent execution without a terminal capture produces a conclusive failure.
Unavailable or conflicting evidence retains the receipt and the attempt fence.
Legacy receipts lack the identity and keep the existing operator recovery path.

### ADR-0006: Seal upload capacity is isolated from agent work

`seal-push` gets its own single cross-process scheduler lease and is selected
alongside, rather than within, the configured ordinary capacity. Its offsite
copy is a durability boundary, while ordinary slots commonly hold multi-minute
Herdr agent runs; sharing the pool can silently defer seal uploads past their
90-minute broker-contact alert threshold.

The dedicated lease remains one-at-a-time and does not share `health`'s
supervisory lease. Health observes a failure; `seal-push` prevents the failure,
so either must remain runnable while the ordinary queue is saturated.
