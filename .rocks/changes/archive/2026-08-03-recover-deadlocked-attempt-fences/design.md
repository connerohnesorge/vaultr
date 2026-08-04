## Implementation Details

### Block reasons

`reconcile_fence_at` collapsed `ReceiptLookup::Absent` and `Pending` into one `Ok(_)`
arm. They stay equally blocking, but they are different facts and now say so, and both
name the remedy. `reconcile_fence_at` gains the job name so it can print the command.

### Operator unblock

```rust
pub enum Unblocked { NoFence, AlreadyClear, Cleared(String) }
pub fn unblock_job(name: &str) -> io::Result<Unblocked>;
```

In order:

1. Take the attempt flock — the same one a dispatch takes, so this cannot race a live
   tick. A held lock is an error, not a force.
2. No fence → `NoFence`, exit 0. Idempotent; running it twice is not an error.
3. `reconcile_fence` returns `Ready` → `AlreadyClear`, exit 0, and say the next tick
   handles it. Never force what resolves itself.
4. Otherwise append one `failed` record — `unblocked by operator: attempt <id> abandoned
   without a durable outcome` — then remove the fence and `sync_dir`.

`failed` is the honest outcome: the attempt really did not complete. It also makes the
event visible to the health sweep, which is the half of this incident that let it run
for five days.

## Goals / Non-Goals

- Goal: every terminal fence state has a documented, audited exit that is not `rm`.
- Goal: an abandoned attempt appears in the job ledger, so "blocked" stops looking like
  "idle" to `health.15m.sh`.
- Non-Goal: auto-clearing any fence that does not clear today. See ADR-0001 — the first
  draft of this change did exactly that and was unsound.
- Non-Goal: auto-clearing a pending fence after some age. Any threshold is a guess about
  whether a possibly-live agent is dead, which is the ambiguity the fence exists to
  refuse.
- Non-Goal: a `plant jobs fences` listing. The block reason now names the remedy, and the
  fences are one `ls` of `job-attempts/`.

## Decisions

- The operator command records `failed` rather than clearing silently. A silent clear
  would restore the job but leave the outage invisible, which is the failure this change
  exists to fix.
- `AlreadyClear` is a refusal, not a fallback. If reconciliation can resolve a fence, it
  owns it; the operator path must not append a competing record.

## ADRs

### ADR-0001: An absent receipt keeps blocking; recovery is an operator command, not an inference

The obvious fix for the incident was to make an absent receipt clear the fence
automatically. `claim_agent_run` fsyncs its `InProgress` record before the agent
executes, so an absent receipt looks like proof the run never started, and auto-recovery
would need no human at all. This change was implemented that way first and it was wrong.

An absent receipt does not mean "nothing ran". It means "no keyed agent run is claimed",
which is also true of two safe-looking cases:

- A job that dispatches no agent never writes a receipt at all. Most jobs are like this.
  `scheduled_record_failure_blocks_the_next_dispatch` covers exactly this shape: a plain
  script job runs, its final ledger append fails, and the retained fence is what stops
  the script running a second time. Auto-clearing re-ran it — the test caught it.
- `run_agent` returning `Unavailable` deletes the claim on purpose, so the idempotency
  key stays reusable for the retry. That path ends in an absent receipt after a claim.

So absence cannot distinguish "never started" from "ran, outcome unproven", and the
existing requirement to retain is correct. What was missing was never the inference — it
was an exit.

Decision: reconciliation semantics are unchanged. Add `plant jobs unblock <name>`, name
it in every block reason, and record the abandonment as a `failed` ledger entry so the
health sweep sees it. Consequence: recovery stays a deliberate human act, which is right
for a state whose whole purpose is refusing to guess — but it is now a documented,
audited, discoverable one instead of hand-deleted state.

## Risks / Trade-offs

- The operator could unblock a job whose agent is genuinely still running, orphaning it.
  → The command takes the attempt flock, so it cannot race a live dispatch, and it
  refuses when reconciliation would already clear the fence. The residual case is a live
  agent whose scheduler died, which is exactly when the operator wants to intervene.
- Recovery still needs a human, so a wedge still costs time-to-notice. → The `failed`
  record makes `health.15m.sh` alert on the job instead of reporting it silent, which
  cuts the five-day discovery gap. Automatic recovery is left to a change that can prove
  it is safe.

## Open Questions

- Should `Unavailable` persist a terminal "never launched" receipt instead of deleting
  the claim? That would make its fences self-reconciling, but a terminal receipt under
  the same key would also make the next `plant agent run` refuse to retry. Needs its own
  design.
