# Plant Agent Jobs Specification

## Requirements

### Requirement: Deep Herdr agent lifecycle

Plant MUST run each agent-backed Cultivation Job through one high-level Herdr lifecycle interface that owns workspace creation and reclamation, verified agent readiness, prompt delivery, completion waiting, and best-effort cleanup with focus restoration. Plant MUST reconcile readiness snapshots until the selected supported native-agent pane remains prompt-ready after the composer settles. Plant MUST snapshot that pane's terminal and available agent-session identity before checked `pane run` prompt typing. Plant MUST receive a subscription acknowledgment before prompt typing. If the pane has not entered `working`, Plant MUST send exactly one checked Enter. Plant MUST reconcile buffered lifecycle events with same-pane snapshots. Plant MUST observe post-submit `working` before accepting a later terminal state. Plant MUST recheck terminal/session identity before returning success. Failed typing, failed submission, a missing transition, pre-existing terminal state, or a pane identity change MUST NOT return `Succeeded`. The scheduler MUST retain job selection, launch construction, prompts, cadence, and outcome recording.

#### Scenario: Herdr is unavailable before an attempt

- WHEN the initial Herdr availability check fails
- THEN the lifecycle returns `Unavailable`
- AND Plant does not record an attempt so a later scheduler tick may retry

#### Scenario: Startup or prompt delivery fails

- WHEN Herdr is available but workspace creation, CLI startup, subscription acknowledgment, prompt typing, or required Enter delivery fails
- THEN the lifecycle returns `Failed` with a diagnostic detail
- AND Plant records the failed attempt using the existing cadence policy

#### Scenario: Agent run succeeds

- WHEN a verified supported native-agent pane is idle or done
- AND the lifecycle acknowledges its pane-scoped status subscription before checked `pane run` prompt typing
- AND the lifecycle sends one checked Enter only if the pane is not already `working`
- AND Plant observes `working` followed by a terminal state through its buffered stream or same-pane snapshots
- AND the terminal and available agent-session identities remain unchanged
- THEN the lifecycle returns `Succeeded` with a diagnostic detail
- AND applies the supplied `Never`, `Always`, or `OnSuccess` cleanup policy without changing user focus

#### Scenario: Pre-existing done is not this run

- WHEN the selected pane is already `done` before prompt submission but no post-acknowledgment `working` transition is observed
- THEN the lifecycle returns `Failed`
- AND Plant MUST NOT durably report the run as succeeded

#### Scenario: Readiness changes while the composer settles

- WHEN a selected supported native-agent pane becomes non-ready after initial readiness
- THEN Plant continues waiting for the same pane identity
- AND Plant does not type the prompt before readiness returns

#### Scenario: A terminal event is absent

- WHEN Plant observes post-submit `working`
- AND a same-pane snapshot later reports `idle` or `done`
- AND the terminal event is absent from the subscription
- THEN Plant returns success after identity verification

#### Scenario: Pane identity changes during the turn

- WHEN the pane's terminal or captured agent-session identity changes after submission
- THEN the lifecycle returns `Failed` even if a terminal status is observed
- AND Plant MUST NOT durably report the run as succeeded

#### Scenario: Agentless or unknown pane is observed

- WHEN the selected pane is agentless, unknown, or not a supported native-agent pane in idle or done state
- THEN Plant MUST NOT deliver the prompt
- AND the lifecycle fails with diagnostic detail while applying its cleanup policy

#### Scenario: Cleanup fails after success

- WHEN the agent succeeds but stale-workspace reclamation or final cleanup fails
- THEN the lifecycle remains `Succeeded`
- AND cleanup failure does not expand the outcome contract or change scheduler recording

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

Plant MUST acquire scheduler capacity before it publishes a scheduled attempt fence.
After capacity admission, Plant MUST acquire the per-job cross-process flock.
While holding the flock, Plant MUST verify ledger writability, reconcile prior state, and recheck cadence.
Plant MUST publish the fence immediately before the scheduled action.
Every new attempt fence MUST persist its scheduled action kind.
The supported action kinds MUST be `Script` and `InProcessCompression`.
An absent action kind MUST identify a legacy fence.
Restart-independent script workers MUST NOT execute `InProcessCompression`.
Plant MUST retain the flock through one durable final-record or retryable-fence transition.
Plant MUST retain a nonretryable fence whenever the final outcome cannot be proven durable.
On daemon cancellation, Plant MUST stop both listener supervisors from accepting.
Plant MUST boundedly drain their pre-cancellation connections and owned capture descendants.
Plant MUST abort and reap leftovers before releasing either listener lease.
A capture tee MUST stop pulling a stalled upstream when its client receiver closes.

#### Scenario: State is unavailable before dispatch

- WHEN the attempt directory, job ledger, or required durability operation is unavailable
- THEN Plant fails closed before executing a script or in-process action
- AND no job side effect occurs

#### Scenario: Concurrent schedulers observe one due period

- WHEN two scheduler processes evaluate the same due job
- AND both processes obtain local scheduler capacity
- THEN only the process holding the per-job flock may reconcile and recheck cadence
- AND at most one process publishes a fence and dispatches that due period

#### Scenario: Plant stops during capacity waiting

- GIVEN all scheduler capacity is occupied
- WHEN Plant stops before a due job receives capacity
- THEN the waiting job has no nonretryable attempt fence
- AND the next Plant process can evaluate the job on its next scheduler tick

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
- THEN Plant publishes an `InProcessCompression` fence inside the same held attempt guard
- AND Plant dispatches typed `InProcessCompression` through that guard
- AND never executes the wrapper or records through an unfenced path

#### Scenario: Manual compression uses script identity

- WHEN an operator runs `plant jobs run compress`
- THEN Plant publishes a `Script` fence through normal attempt fencing
- AND Plant executes the manual wrapper

#### Scenario: A legacy fence has no action identity

- GIVEN a previous Plant version published an actionless fence
- WHEN Plant deserializes that fence
- THEN Plant identifies it as legacy state
- AND Plant does not infer an action kind from the current job name

#### Scenario: Script workers cannot own compression

- WHEN Plant dispatches scheduled work through a restart-independent script worker
- THEN the worker accepts only a `Script` action
- AND the listener-owning daemon retains every `InProcessCompression` action

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

### Requirement: Scheduled attempt receipt reconciliation

Plant MUST export the published attempt ID to each job script as
`PLANT_ATTEMPT_ID`. Plant MUST read the keyed Agent Run receipt for an
unresolved nonretryable fence when the job ledger holds no matching record.
Plant MUST append one durable final ledger record for a conclusive receipt
before it clears that fence. Plant MUST retain the fence for an absent,
pending, unreadable, or mismatched receipt. Plant MUST NOT start another Herdr
lifecycle during reconciliation.

#### Scenario: A job script receives its attempt ID

- WHEN Plant executes a scheduled or manual job script
- THEN the script environment holds `PLANT_ATTEMPT_ID` with the published fence ID
- AND an agent-backed script can supply that value as its Agent Run idempotency key

#### Scenario: A succeeded receipt reconciles a stranded fence

- WHEN an unresolved nonretryable fence has no matching ledger record
- AND the Agent Run receipt for that attempt ID records a succeeded outcome
- THEN Plant appends one durable final ledger record with the `success` outcome
- AND Plant clears the fence without another Herdr launch

#### Scenario: A failed receipt reconciles a stranded fence

- WHEN an unresolved nonretryable fence has no matching ledger record
- AND the Agent Run receipt for that attempt ID records a failed outcome
- THEN Plant appends one durable final ledger record with the `failed` outcome
- AND Plant clears the fence without another Herdr launch

#### Scenario: The receipt is absent

- WHEN an unresolved nonretryable fence has no matching ledger record
- AND no Agent Run receipt exists for that attempt ID
- THEN Plant retains the fence
- AND Plant does not redispatch the job

#### Scenario: The receipt is pending

- WHEN the Agent Run receipt for the fence attempt ID remains in progress
- THEN Plant retains the fence
- AND Plant does not redispatch the job

#### Scenario: The receipt is unreadable

- WHEN the Agent Run receipt for the fence attempt ID is corrupt
- THEN Plant retains the fence
- AND Plant reports the unreadable receipt as the block reason

### Requirement: Recoverable keyed Agent Run identity

Plant MUST persist a tagged phase-specific checkpoint in each keyed Agent Run
receipt. The checkpoint MUST preserve the immutable Herdr workspace and pane
identity. The checkpoint MUST represent terminal-only and session-bound pane
identity as distinct states. A captured checkpoint MUST include the captured
session. Plant MUST reconcile a pending receipt against the exact checkpoint
identity. Plant MUST NOT use receipt age as execution evidence. Plant MUST NOT
create another workspace while the recorded execution can still finish. Plant
MUST retain the fence when Herdr or identity evidence is unavailable. Legacy
pending receipts MUST require verified operator recovery. The Codex Learn
wrapper and 15-minute cadence MUST remain unchanged.

#### Scenario: The recorded pane remains working

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- AND the exact recorded pane remains working
- WHEN Plant reconciles the receipt
- THEN Plant resumes observation of that pane
- AND Plant does not create another Herdr workspace

#### Scenario: The recorded session completed

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- AND a captured checkpoint proves submitted work
- AND the matching captured session contains a terminal response
- WHEN Plant reconciles the receipt
- THEN Plant persists one conclusive successful receipt
- AND Plant appends one successful job ledger record
- AND Plant clears the attempt fence

#### Scenario: The recorded execution cannot finish

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- AND Herdr proves the recorded execution is absent
- AND the matching captured session has no terminal response
- WHEN Plant reconciles the receipt
- THEN Plant persists one conclusive failed receipt
- AND Plant appends one failed job ledger record
- AND Plant clears the attempt fence

#### Scenario: Recovery evidence is unavailable

- GIVEN Plant restarts with a pending keyed Agent Run receipt
- WHEN Herdr is unavailable or the recorded identity conflicts
- THEN Plant retains the pending receipt
- AND Plant retains the attempt fence
- AND Plant does not create another Herdr workspace

#### Scenario: A legacy pending receipt lacks identity

- GIVEN a pending keyed Agent Run receipt stores only its attempt key
- WHEN Plant reconciles the receipt
- THEN Plant retains the attempt fence
- AND Plant names `plant jobs unblock <name>` as the operator recovery

#### Scenario: A terminal-only checkpoint awaits session discovery

- GIVEN a pending keyed Agent Run receipt stores a terminal-only checkpoint
- WHEN the captured session identifier is not available
- THEN Plant retains the pending receipt
- AND Plant does not infer a capture session

#### Scenario: Checkpoint identity is stable

- GIVEN a pending keyed Agent Run receipt stores a checkpoint
- WHEN a later progress update supplies a different workspace, pane, terminal, or session identity
- THEN Plant rejects the update
- AND Plant retains the existing checkpoint

#### Scenario: The current Codex Learn attempt is recovered

- GIVEN session `019fb277-d08d-7f62-a1dd-2115d251056e` contains a terminal response
- AND no live execution owns attempt `ddd1fb63-2eb6-4c17-8bb3-a882f1c497ef`
- WHEN the operator runs `plant jobs unblock learn-codex`
- THEN Plant records the abandoned attempt as failed
- AND a later Codex Learn run can write a durable final record

#### Scenario: Codex Learn scheduling remains unchanged

- GIVEN interrupted Agent Run recovery is deployed
- WHEN Plant scans `vault/jobs/learn-codex.15m.sh`
- THEN Plant schedules Codex Learn every 15 minutes

### Requirement: Pi agent job launches

`plant agent run` MUST accept `--cli pi`. Usage and invalid-value errors MUST list `pi` as a supported launch identity. Plant MUST map this identity to Herdr's `pi` agent. Pi panes MUST participate in all supported native-agent pane gates and recovery checks. Plant MUST render Pi launches with `PLANT_AGENT=1`, project trust approval, the `openai-codex` provider, Pi's `--model` option, and Pi's `--thinking` option. Plant MUST give every Pi run a unique `--session-dir` under Plant state. Plant MUST read the first JSONL record ID from that directory and register it as the job self-capture before workspace cleanup.

#### Scenario: Pi is selected

- WHEN an operator supplies `plant agent run --cli pi`
- THEN Plant accepts the command
- AND usage and invalid-value errors list `pi`
- AND Plant expects Herdr to report the pane agent as `pi`

#### Scenario: Pi launch options are rendered

- WHEN Plant launches Pi with a model and effort
- THEN the launch starts with `PLANT_AGENT=1 command pi --approve --provider openai-codex`
- AND Plant renders the model with `--model`
- AND Plant renders the effort with `--thinking`

#### Scenario: Pi run session is isolated

- WHEN Plant prepares a Pi run
- THEN Plant appends a unique `--session-dir` under Plant state
- AND no other prepared Pi run receives the same directory

#### Scenario: Pi self-capture is registered

- WHEN the sole Pi JSONL session file starts with `{"type":"session","id":"<uuid>"}`
- AND the Pi run reaches a terminal state
- THEN Plant registers that record ID as the job self-capture before cleanup

#### Scenario: Native effort flags are smuggled through extra arguments

- WHEN Pi extra arguments contain `--thinking`
- THEN Plant rejects the command
- AND the operator must use `--effort`

#### Scenario: Existing launch identities remain supported

- WHEN Plant launches Claude, Codex, or Prime
- THEN Plant preserves the existing launch and self-capture behavior

### Requirement: Fair scheduled admission

Plant MUST keep a rotating queue of due ordinary jobs. Each scheduler scan MUST launch no more ordinary workers than configured capacity. A capacity rejection MUST return the selected job to the queue after other due jobs. Plant MUST acquire cross-process capacity before it publishes an attempt fence. Plant MUST preserve per-job locks and durable cadence rechecks.

#### Scenario: One slot serves multiple due jobs

- GIVEN one ordinary scheduler slot is available
- AND multiple ordinary jobs are due
- WHEN Plant completes successive scheduler scans
- THEN each due job receives an admission turn

#### Scenario: Capacity remains occupied

- GIVEN all ordinary scheduler slots are occupied
- WHEN a selected due job cannot acquire capacity
- THEN Plant records a diagnostic with the job name
- AND Plant gives another due job the next admission turn
- AND Plant publishes no attempt fence for the rejected job

#### Scenario: One job has an unresolved fence

- GIVEN one due job has an unresolved attempt fence
- WHEN another job is due
- THEN Plant keeps both jobs in fair admission
- AND the unrelated job can execute

#### Scenario: Plant restarts before admission

- WHEN Plant restarts before a selected job receives capacity
- THEN Plant rebuilds admission from durable cadence state
- AND Plant publishes no attempt fence before capacity admission

### Requirement: Owned learner batch startup boundary

Plant MUST store the inherited `PLANT_ATTEMPT_ID` with a claimed learner batch. Claude and Codex learner batches MUST remain independent. Plant MUST retry the initial Herdr availability probe with a bounded backoff budget. Plant MUST release only the learner batch owned by the matching attempt when that budget expires before workspace creation. Plant MUST retain the batch after workspace creation or any uncertain outcome. Plant MUST preserve keyed Agent Run receipt behavior.

#### Scenario: Herdr recovers during pre-launch retry

- WHEN an initial Herdr availability probe fails
- AND Herdr responds within the bounded retry budget
- THEN Plant creates one Agent Run workspace
- AND Plant retains the learner batch until normal completion

#### Scenario: Herdr remains unavailable before workspace creation

- GIVEN a learner batch is owned by the current `PLANT_ATTEMPT_ID`
- WHEN every pre-launch availability probe fails
- THEN Plant releases that exact learner batch
- AND Plant returns a retryable Agent Run receipt

#### Scenario: Another learner owns a batch

- GIVEN another learner batch has a different owner
- WHEN the current attempt fails before workspace creation
- THEN Plant retains the differently owned batch

#### Scenario: Startup outcome is uncertain

- WHEN Plant creates a workspace or cannot prove pre-workspace failure
- THEN Plant retains the learner batch
- AND Plant retains fail-closed Agent Run state
