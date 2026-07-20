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

### ADR-0002: Scheduled attempts are fenced before waiting or side effects

A scheduled dispatch holds one per-job attempt guard from a locked durable
cadence recheck, before semaphore waiting or any job side effect, through the
typed action and exactly one durable final or retryable transition. If the
guard or its state cannot be loaded or published safely, dispatch fails closed.
This makes one due period one scheduled attempt even with concurrent schedulers
or a long capacity wait, while a retryable result deliberately rearms it.

The process deadline covers the direct child and both captured output drains,
so a descendant retaining an output pipe cannot strand the guard. Capture work
uses the same attempt fence, but listener retention and capture-descendant
draining remain owned by `capture-stewardship/design.md` ADR-0001.
