# Design

## Why the batch is all-or-nothing today

`recover` builds an inventory of every Session Capture that needs work, then
applies each one in a single loop. The loop uses `?`, so the first failure
returns. Startup recovery and the periodic sweep share this one function. The
strictness is correct for startup and wrong for the sweep.

Startup recovery gates traffic. Serving over evidence Plant cannot verify risks
corrupting Envelope order, so failing closed is right. The periodic sweep runs
while Plant already serves traffic. Its only job is to unblock stranded drains.
Failing closed there converts one damaged Session Capture into total loss of the
liveness guarantee for every live Session Capture.

The fix splits the failure policy, not the transaction. Both paths keep the same
per-session logic. Only the handling of a per-session error differs.

## Quarantine over deletion

Session Capture bytes are append-only evidence. A stage that cannot be
reconciled is still real observed wire data. Deleting it destroys evidence and
hides the incident. Leaving it in place blocks the drain forever, which is the
current failure.

Quarantine moves the file under a sibling directory of the staging root and
leaves the bytes untouched. The drain proceeds. The loss is recorded through the
existing `record_drop` path, so `.meta` carries `dropped_turns`, a reason, and a
timestamp. An operator can inspect or replay the quarantined evidence later.

## Accounting the stranded backlog

Drop accounting currently covers a failed preparation and a failed completion.
It does not cover an Envelope that staged cleanly and never drained. That is the
exact shape of the observed loss, which is why every audited Session Capture
reports zero dropped turns while turns are missing.

Adding a stranded backlog count to `/health` and to `.meta` closes the honesty
gap. A future capture that loses turns says so.

## ADRs

### ADR-0001: Split recovery failure policy by caller, not by error class

The sweep and startup recovery share one transaction. Classifying every error as
recoverable or fatal would require judging each error site and would drift as
new errors appear.

Instead the caller chooses the policy. Startup recovery fails on the first
error. The periodic sweep records every per-session error, continues to the next
Session Capture, and returns an aggregate failure. The per-session logic stays
identical, so there is one code path to reason about.

The trade-off is that the sweep can repeatedly retry a Session Capture that will
never succeed. Quarantine bounds that retry, because an irreconcilable stage
moves aside on the first sweep that reaches it.

### ADR-0002: Treat absent stage removal as success

`commit_stage` deletes a stage file as its final act. Mapping a not-found error
to a failure treats the intended end state as an error.

Removal becomes idempotent. A not-found error is success. Every other removal
error still fails, because it signals a real storage or permission fault.
