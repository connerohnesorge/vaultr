# Plant Agent Jobs — Delta

## ADDED Requirements

### Requirement: Polyglot shebang job execution

Plant MUST discover jobs as executable files matching
`<name>.<interval>.<ext>` for any extension and MUST execute each job by
exec'ing its path directly so the script's shebang selects the interpreter.
Plant MUST NOT hardwire or map interpreters per extension.

#### Scenario: TypeScript job runs via its shebang

- WHEN `door-oncall.30m.ts` is executable with a `#!/usr/bin/env bun` shebang
- THEN the scanner registers it with a 30-minute cadence
- AND Plant executes it directly, with Bun chosen by the shebang

#### Scenario: Non-job files are skipped

- WHEN a file in the jobs directory lacks an interval segment (e.g. `AGENTS.md`)
- THEN the scanner skips it, as today

#### Scenario: Shebang-less scripts keep working

- WHEN an executable job has no shebang
- THEN the OS exec fallback runs it via `/bin/sh`
- AND legacy shell jobs need no migration

#### Scenario: Unspawnable job fails visibly

- WHEN a job cannot be spawned (e.g. missing execute permission)
- THEN Plant records a failed attempt with the spawn error in the job ledger
- AND the scheduler continues unaffected

### Requirement: Durable scheduled-job attempt fencing

Plant MUST acquire a per-job cross-process flock, reconcile any prior attempt,
recheck cadence from the durable ledger while still holding that flock, and
durably publish a unique attempt fence before waiting for scheduler capacity or
performing any job side effect. Every terminal execution path MUST pass one
typed result through exactly one durable final-record transition or one
retryable-fence transition. Plant MUST retain a nonretryable fence whenever the
final outcome cannot be proven durable. On daemon cancellation, Plant MUST
stop both listener supervisors from accepting, freeze and boundedly drain only
their pre-cancellation connection sets and owned capture tee/finalizer task
sets, abort and reap leftovers, and retain both listener leases until both
supervisors join. A capture tee MUST stop pulling a stalled upstream when its
client receiver closes.

#### Scenario: State is unavailable before dispatch

- WHEN the attempt directory, job ledger, or required durability operation is unavailable
- THEN Plant fails closed before executing a script or in-process action
- AND no job side effect occurs

#### Scenario: Concurrent schedulers observe one due period

- WHEN two scheduler processes evaluate the same due job
- THEN only the process holding the per-job flock may reconcile and recheck cadence
- AND at most one process publishes a fence and dispatches that due period

#### Scenario: Retryable exit rearms the attempt

- WHEN a job exits with status 75
- THEN Plant durably marks the existing fence retryable without appending a terminal ledger record
- AND a later scheduler tick may reconcile that fence and retry

#### Scenario: Crash leaves an unresolved fence

- WHEN Plant crashes after fence publication and before a terminal record is durable
- THEN restart retains the nonretryable fence
- AND Plant does not immediately redispatch the job

#### Scenario: Matching recovery record is durable

- WHEN restart finds a ledger line with the unresolved fence's attempt ID
- THEN Plant parses that line through a retained regular descriptor
- AND successfully fsyncs that exact descriptor and the jobs directory before clearing the fence

#### Scenario: Matching recovery record cannot be synced

- WHEN matching ledger bytes are visible but the ledger descriptor or jobs directory cannot be synced
- THEN Plant retains the nonretryable fence
- AND it does not redispatch the job

#### Scenario: Final recording fails

- WHEN execution reaches a terminal result but appending or syncing its ledger record fails
- THEN Plant returns failure and retains the nonretryable fence
- AND the job is not immediately redispatched

#### Scenario: A descendant retains a job output pipe

- WHEN the direct job child exits but a descendant keeps stdout or stderr open
- THEN one deadline bounds the child wait and both output drains
- AND expiry kills and reaps the direct child before the typed failure is finalized through the held attempt guard

#### Scenario: Scheduled compression uses the same lifecycle

- WHEN the exact `compress` job is due in the listener-owning daemon
- THEN Plant dispatches typed `InProcessCompression` inside the same held attempt guard
- AND never executes the wrapper or records through an unfenced path
- BUT a manual `plant jobs run compress` executes the wrapper through normal attempt fencing

#### Scenario: Compression owns the complete capture boundary

- WHEN Plant starts its daemon or runs `plant compress once`
- THEN it acquires both capture listeners or releases any partial bind before mutation
- AND performs startup/offline capture recovery once while retaining both leases
- AND scheduled compression sweeps never invoke capture recovery

#### Scenario: Daemon shutdown freezes and drains accepted work

- WHEN the listener-owning daemon receives cancellation
- THEN both listener supervisors stop accepting new work
- AND each supervisor closes its capture-task tracker and drains its fixed connection and capture-descendant sets to one bounded deadline
- AND it aborts and reaps every leftover connection, tee, and finalizer before returning
- AND both listener leases remain held until both supervisors have joined

#### Scenario: Client disconnects from an indefinitely stalled captured stream

- WHEN a captured response has started but its upstream stalls indefinitely and the client disconnects
- THEN the capture tee observes receiver closure and stops pulling the upstream
- AND cancellation drains or aborts and reaps its finalizer before the listener lease returns

### Requirement: Idempotent Plant agent runs

`plant agent run` MUST accept an optional stable idempotency key, durably claim
that key before starting the Herdr lifecycle, and durably record each
conclusive `Succeeded` or `Failed` outcome before returning. A repeated key
MUST return its prior durable outcome without creating another Herdr workspace.
Unreadable, corrupt, or unresolved in-progress idempotency state MUST fail
closed without launching. Plant MUST durably link newly created state
directories before publishing a job fence, Agent Run claim, or outcome. The
command MUST directly serialize one tagged `AgentRunReceipt` enum when an
idempotency key is supplied; its variant determines exit status and durability.
It MUST NOT independently serialize state, durability, and exit-code fields
that can disagree or report claim or persistence failures as a conclusive
`Failed` outcome. Without an idempotency key, the command MUST preserve its
legacy human status line and exit mapping instead of emitting receipt JSON.

#### Scenario: An unkeyed agent run uses the legacy CLI contract

- WHEN `plant agent run` is invoked without an idempotency key
- THEN its final stdout line is `[agent:<label>] succeeded: <detail>`, `[agent:<label>] herdr unavailable`, or `[agent:<label>] failed: <detail>`
- AND it exits 0, 75, or 1 respectively
- AND it does not emit an `AgentRunReceipt`

#### Scenario: A completed agent run is retried

- WHEN `plant agent run` receives a key whose conclusive outcome is already durable
- THEN it returns the recorded outcome
- AND it does not create or reclaim a Herdr workspace

#### Scenario: Duplicate launch state is uncertain

- WHEN a key is already claimed without a conclusive durable outcome
- THEN `plant agent run` reports an indeterminate non-durable result and fails closed
- AND it does not create another Herdr workspace

#### Scenario: Herdr is unavailable before launch

- WHEN the initial Herdr availability probe returns `Unavailable`
- THEN Plant removes the key's pre-launch claim durably
- AND reports a retryable non-durable result
- AND a later call with the same key may retry

#### Scenario: Conclusive outcome cannot be persisted

- WHEN the Herdr lifecycle finishes but Plant cannot durably persist its conclusive outcome
- THEN Plant reports an indeterminate non-durable result
- AND it does not misreport a durable `Failed` outcome

#### Scenario: Agent Run state directory is new

- WHEN Plant must create one or more missing state-directory levels before publishing a claim
- THEN it fsyncs each new directory and its parent before starting Herdr
- AND the claim is not published through an unlinked directory entry

## MODIFIED Requirements

### Requirement: Deep Herdr agent lifecycle

Plant MUST run each agent-backed Cultivation Job through one high-level Herdr lifecycle interface that owns workspace creation and reclamation, verified agent readiness, prompt delivery, completion waiting, and best-effort cleanup with focus restoration. After the final ready check, it MUST snapshot the selected native Claude/Codex pane's terminal and available agent-session identity, receive a subscription acknowledgment before exactly one checked atomic `pane run` prompt submission, observe that same pane/workspace enter `working` after the acknowledgment, observe its later `done` on the same buffered subscription, and recheck terminal/session identity before returning success. A failed submission, missing transition, pre-existing terminal state, or pane identity change MUST NOT return `Succeeded`. The scheduler MUST retain job selection, launch construction, prompts, cadence, and outcome recording.

#### Scenario: Herdr is unavailable before an attempt

- WHEN the initial Herdr availability check fails
- THEN the lifecycle returns `Unavailable`
- AND Plant does not record an attempt so a later scheduler tick may retry

#### Scenario: Startup or prompt delivery fails

- WHEN Herdr is available but workspace creation, CLI startup, subscription acknowledgment, or the single atomic prompt submission fails
- THEN the lifecycle returns `Failed` with a diagnostic detail
- AND Plant records the failed attempt using the existing cadence policy

#### Scenario: Agent run succeeds

- WHEN a verified native Claude or Codex pane is idle or done
- AND the lifecycle acknowledges its pane-scoped status subscription before exactly one checked `pane run`
- AND that same buffered stream observes `working` followed by `done`
- AND the terminal and available agent-session identities remain unchanged
- THEN the lifecycle returns `Succeeded` with a diagnostic detail
- AND applies the supplied `Never`, `Always`, or `OnSuccess` cleanup policy without changing user focus

#### Scenario: Pre-existing done is not this run

- WHEN the selected pane is already `done` before prompt submission but no post-acknowledgment `working` transition is observed
- THEN the lifecycle returns `Failed`
- AND Plant MUST NOT durably report the run as succeeded

#### Scenario: Pane identity changes during the turn

- WHEN the pane's terminal or captured agent-session identity changes after submission
- THEN the lifecycle returns `Failed` even if a terminal status is observed
- AND Plant MUST NOT durably report the run as succeeded

#### Scenario: Agentless or unknown pane is observed

- WHEN the selected pane is agentless, unknown, or not a native Claude/Codex pane in idle or done state
- THEN Plant MUST NOT deliver the prompt
- AND the lifecycle fails with diagnostic detail while applying its cleanup policy

#### Scenario: Cleanup fails after success

- WHEN the agent succeeds but stale-workspace reclamation or final cleanup fails
- THEN the lifecycle remains `Succeeded`
- AND cleanup failure does not expand the outcome contract or change scheduler recording
