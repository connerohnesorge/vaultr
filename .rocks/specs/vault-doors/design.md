# Vault Doors Design

## ADRs

### ADR-0001: Jobs exec via shebang, not a hardwired interpreter

Plant executes an executable job path directly and lets its shebang select the
runtime; filename parsing remains extension-agnostic. Plant therefore learns
nothing about Bun, Python, or future runtimes, and existing shebang-less jobs
retain the platform exec fallback. Mapping extensions to interpreters inside
Plant was rejected because it would create recurring scheduler special cases.

### ADR-0002: Doors are jobs, not a separate engine

A Door is a Cultivation Job: filename cadence, dispatch, and outcome recording
stay in Plant, while typed Door code owns filters and prompt construction. A
resident Door engine would duplicate the scheduler and require a filter DSL;
per-Door shell implementations would duplicate the fencing whose failure can
launch an agent twice. Plant's durable dispatch fence remains the single
implementation described by `plant-agent-jobs/design.md` ADR-0002.

### ADR-0003: The TypeScript library wraps Plant's idempotent agent-run boundary

The public library keeps its legacy unkeyed `agentRun` and uppercase
three-state `AgentOutcome` compatibility. Door uses a separate typed receipt
client with the persisted batch's stable idempotency key, and only Plant's
serialized receipt determines retryability, indeterminacy, or a durable
conclusive outcome. Door advances no claim for a retryable or indeterminate
result. Plant remains the sole owner of durable key transitions and the Herdr
lifecycle; see `plant-agent-jobs/design.md` ADR-0001.

### ADR-0004: Loop prevention is allowlist-by-construction plus a rate breaker

Doors may watch only ingestion roots that agents do not write. This structural
partition prevents a Door-launched agent from sustaining its own trigger when
external write attribution is unavailable. A rolling-window fire breaker
pauses excessive firing and requires manual re-arm as defense in depth.
Consecutive-fire detection was rejected because normal agent duration can make
a real loop slow enough to evade it; provenance tagging was disproportionate
once watch roots were partitioned.

### ADR-0005: Door batches are ordered durable claims

Each Door serializes evaluation with a cross-process lock and atomically
persists a timestamp frontier, every path already seen at that timestamp, and
the exact in-progress batch ordered by `(mtime,path)`. The claim binds stable
descriptor identity, size, and full-content digest before launch and supplies
Plant's idempotency key. Retry resumes the same claim and key; only a durable
Plant `Succeeded` or `Failed` outcome advances the frontier. This handles
timestamp ties, late files at the frontier, pathname replacement, and crashes
on either side of launch without firing twice.

State, locks, claim hydration, and mail projection share one root-bound,
no-follow directory abstraction that retains and revalidates the canonical
directory identity for bounded reads, publication, rename, unlink, and fsync.
Unreadable or invalid state, unsafe traversal, descriptor drift, aliases,
non-regular nodes, and publication uncertainty fail closed. Shipped scalar and
v1 cursors migrate under that lock before evaluation, preserving their known
never-fire-twice boundary and any historical in-progress Plant key.
