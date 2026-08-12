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


### ADR-0006: Rotate due jobs before worker launch

The scheduler keeps a process-local queue of due ordinary job names. Each scan refreshes membership without resetting the current order. The scheduler launches only the configured number of admission turns. A rejected job returns behind the other due jobs. Restart rebuilds the queue from durable cadence state.

Cross-process capacity admission remains before the per-job attempt guard. The worker performs the durable cadence recheck after admission. Capacity rejection writes a job-named diagnostic without creating a fence or ledger outcome. Unreadable attempt state remains fail-closed.

### ADR-0007: Own learner batches with the scheduled attempt

A learner claim stores the inherited `PLANT_ATTEMPT_ID` as its owner. Claude and Codex use separate lease files and locks. The Agent Run idempotency key uses the same value.

The initial Herdr workspace-list probe has a bounded retry budget. Exhaustion before workspace creation proves that no Agent Run effect started. Plant then removes only lease files whose owner equals the keyed attempt. It makes the idempotency key retryable only after the owner-scoped release succeeds.

Workspace creation is the retention boundary. Plant retains the learner lease after workspace creation, prompt delivery, a failed run, or uncertain evidence. This rule prevents a replacement batch from starting while an Agent Run effect can still exist.
