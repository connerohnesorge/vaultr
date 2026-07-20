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

### Requirement: Idempotent Plant agent runs

`plant agent run` MUST accept an optional stable idempotency key, durably claim
that key before starting the Herdr lifecycle, and durably record each
conclusive `Succeeded` or `Failed` outcome before returning. A repeated key
MUST return its prior durable outcome without creating another Herdr workspace.
Unreadable, corrupt, or unresolved in-progress idempotency state MUST fail
closed without launching. The command MUST emit a machine-readable final result
that distinguishes durable `Succeeded`/`Failed` outcomes from retryable and
indeterminate operational state; it MUST NOT report claim or persistence
failures as a conclusive `Failed` outcome.

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
