# Design — Add Door Jobs

## Context

Doors were grilled to a settled shape before this proposal: triggers come only
from vault file changes (sync jobs are the ingestion layer), detection is
poll-on-cadence rather than filesystem watching, and the door logic lives in
typed code rather than bash or a bespoke declarative format.

## Non-goals

- Defending a mail projector publication or stale Door-lock unlink against a
  hostile same-account process that substitutes a path within one individual
  Node path syscall is out of scope. Stable Bun/Node exposes no portable
  `openat`/`linkat`/`renameat` API, and experimental `bun:ffi` is not a
  production dependency. The shipped retained-descriptor and held-directory
  guards prevent static and pre-operation substitution and fail closed under
  deterministic path-swap tests.

## Decision summary

- A door is a job. No door engine, no `vault/doors/*.toml`, no second
  scheduler: Plant's existing filename-cadence scanner runs door scripts like
  any other Cultivation Job, and outcomes land in the existing jobs ledger.
- Scheduled dispatch holds one per-job attempt guard from the locked durable
  cadence recheck through semaphore wait, typed execution, and exactly one
  final or retryable transition. The process runner applies one deadline to
  the direct child plus both output drains so an inherited descendant pipe
  cannot strand the guard.
- The daemon and offline compressor acquire both capture listeners before
  recovery. Startup/offline recovery runs once under those leases; scheduled
  compression only sweeps. On daemon cancellation, each listener supervisor
  stops accepting, closes its owned capture-task tracker, freezes its
  pre-cancellation connection and capture-descendant sets, drains both to one
  deadline, aborts and reaps leftovers, and returns its listener descriptor.
  A tee selects on client receiver closure as well as upstream data. Both
  leases remain held until both supervisors join. A failed partial bind is
  dropped before any mutation.
- The fragile parts — dedup fencing, watch-root policy, fire-rate breaking,
  agent launch — are written once in a shared TypeScript library, not
  re-authored per door. The package is split by those owned concepts:
  agent-run, root-bound state/locking, ingestion, and a thin Door orchestrator;
  test fault injection is internal and absent from the package entry point.
- Door state fails closed, is replaced atomically under a per-door
  cross-process lock, and carries a timestamp frontier with the paths already
  seen at that timestamp plus the exact ordered in-progress batch. The claim
  includes a portable full-content identity captured with bounded memory and deterministically
  supplies Plant's idempotency key, so process crashes or same-path
  replacement cannot create a second agent run with different content.
- Loop prevention is structural first (doors may only watch ingestion roots
  that agents never write) with a rate breaker as defense-in-depth, because a
  self-sustaining door fires slowly (agent runtime spaces the fires) and would
  slide under naive consecutive-fire detection.

## ADRs

### ADR-0001: Jobs exec via shebang, not a hardwired interpreter

Plant currently spawns `/bin/bash <script>` for every job. It will instead
exec the job path directly and let the shebang choose the interpreter, with
the filename parse extension-agnostic (`<name>.<interval>.<ext>`). This is the
whole polyglot mechanism: Plant learns nothing about Bun, Python, or any
future language. Consequence: every job must be executable; measured on macOS,
a shebang-less script does NOT fail — the exec fallback runs it via `/bin/sh` —
so legacy jobs keep working, and the shebang sweep + `vault/jobs/AGENTS.md`
contract update are for explicitness, not survival.
Rejected alternative: mapping extensions to interpreters inside Plant —
recurring special-casing for no added capability.

### ADR-0002: Doors are jobs, not a separate engine

Rejected alternatives: per-door bash scripts (each reimplements fencing —
the exact code that must not be flaky, since a double-fire means duplicate
agent sessions) and a resident door engine reading declarative TOML (a second
scheduler and a filter DSL that TypeScript predicates make unnecessary).
Doors-as-jobs deletes both: cadence comes from the filename, scheduling and
outcome recording come from Plant, and the door file is real code where real
code is wanted (filters, prompt building).

### ADR-0003: TS library wraps idempotent `plant agent run`; lifecycle stays in Plant

The jobs contract already requires agent jobs to drive panes only via
`plant agent run`, and `plant-agent-jobs` made Plant's Herdr lifecycle the
single owner of workspace creation, readiness, delivery, wait, and cleanup.
The library is a thin typed client over that interface: it supplies the
persisted batch's stable idempotency key and parses Plant's serialized
`AgentRunReceipt` tagged enum, whose variant alone determines exit status and
durability, never a second lifecycle implementation. Plant durably claims each
key before Herdr side effects, records conclusive outcomes before returning,
and returns an already-recorded outcome without creating another workspace. A
pre-launch unavailable probe is retryable; pending or inaccessible idempotency
state and outcome-persistence failure are indeterminate. Neither advances Door
state.

Within Plant, `herdr.rs` owns only the Herdr lifecycle, `agent_run.rs` owns the
receipt and durable idempotency transition around that lifecycle, and
`state.rs` owns the durable filesystem operations shared by Agent Runs, jobs,
capture staging, and sweep state. Herdr therefore does not depend back on the
job scheduler for persistence.

After the final native Claude/Codex ready check, the lifecycle snapshots the
pane's terminal and reported agent session, acknowledges one pane-scoped
`pane.agent_status_changed` subscription, and performs exactly one checked
`pane run`. Herdr 0.7.4 submits that text plus Enter atomically. The lifecycle
accepts success only after the same buffered subscription observes `working`
followed by `done`, then rechecks the terminal/session identity. Thus a
pre-existing `done`, failed submission, missing transition, fast
working-to-done turn, or pane reuse cannot be confused with this run.

### ADR-0004: Loop prevention is allowlist-by-construction plus a rate breaker

Door-launched agents write Vault Content; a door watching agent-written paths
(e.g. learnings) would self-sustain, and write-attribution tagging is not
reliably implementable from outside the agent. Instead the library enforces an
allowlist of watchable ingestion roots (paths written only by sync jobs) and
rejects any other watch glob loudly on first run. A rolling-window breaker
(more than N fires per hour pauses the door, logs to its ledger, and requires
manual re-arm) backstops allowlist mistakes. Rejected: consecutive-fire
breakers (real loops fire slowly and never trip them) and provenance tagging
(machinery disproportionate to the residual risk once paths are partitioned).

### ADR-0005: Door batches are ordered durable claims

A scalar mtime misses ties, while a single `(mtime,path)` cursor misses a file
that lands later at the same timestamp with a lower-sorting path. Each door
therefore serializes evaluation with a per-door cross-process lock, fails
closed on unreadable or invalid state, and persists a timestamp frontier plus
the sorted set of paths already seen at that timestamp. It atomically persists
  the exact batch ordered by `(mtime,path)`, including total size and SHA-256 of
  the entire stable descriptor, before launch. That content-bound claim
determines the Plant idempotency key and survives both pre-launch and
post-launch Door crashes. Hydration must reproduce the persisted mtime, size,
and digest before launch. A retry resumes that claim; only a confirmed durable
Plant `Succeeded` or `Failed` outcome advances the frontier and clears the
claim atomically. Retryable and indeterminate results retain the claim and key.
A migrated v1 in-progress claim keeps its historical Plant key, but durably
binds current content identity under the lock before its first post-migration
launch.

The watch resolver selects one configured ingestion root and canonicalizes it.
Both scanning and claim hydration open matches with no-follow semantics, verify
the opened descriptor is a regular file beneath that root, and test frontier
eligibility before reading content. Opens are nonblocking so matching FIFOs
cannot stall a Door. The entire descriptor is streamed through SHA-256 with
bounded memory while only the first 65,536 bytes are retained for filters and
prompts; the read is bracketed by `fstat`, and device, inode, size, mtime, and
ctime must remain identical. Traversal, symlink escapes, in-place writes,
tail-only replacements, and pathname replacement therefore fail closed. Door
tests run under pinned Bun on both Linux and macOS so the
platform-specific lock and descriptor paths stay live. Hidden scanning is
enabled only when the configured glob explicitly names a hidden segment, so
the mail artifact watch can enter `.door` without broadening ordinary watches.

Mail's append-only daily JSONL capture is not itself a Door input. Before the
mail Door evaluates, a mail-specific producer reads only newline-terminated
records under one run-wide byte budget and per-line cap, opening and closing
one source at a time while deferring the single durable state commit until all
sources pass. Its machine-local, gitignored `mail/.door/` state durably records
`initialized` plus each source's relative path, device, inode, and EOF offset.
The `.door` and `summons` directories must be real no-follow directories;
state and existing artifacts are read only through bounded, nonblocking,
no-follow regular-file descriptors. Projection retains stable descriptor and
identity leases for both directories, revalidates their lexical and canonical
identity around every atomic temp/final publication, and proves each exclusive
no-follow temp descriptor landed inside the held directory before writing.
These mechanics live in `safe-loader.ts`: `RootBoundDirectory` retains and
revalidates the private canonical directory descriptor, exclusively creates
and fsyncs retained regular entries, publishes them without replacement, and
atomically replaces+fsyncs state. Door locking/state and mail projection share
that boundary rather than duplicating path publication logic.
Hostile same-account substitution within one individual Node path syscall is
outside the local trust model; static or pre-operation directory replacement
fails closed without publishing outside the held directory. The first true run snapshots all existing
EOFs and publishes nothing; a daily file discovered after initialization
begins at offset zero. A tracked source that disappears, truncates, changes
inode, or is replaced at its open pathname fails closed without advancing any
offset.

Only inbox records with nonempty Graph and trimmed Internet Message IDs are
eligible, and the literal summon is matched only in subject, body preview, or
body content. Each match is durably published as one immutable artifact keyed
by `sha256(trimmed internetMessageId)` before the source-offset update is
atomically saved. A crash in that interval replays the source line, verifies
the existing artifact's trimmed Internet Message ID, and advances without
rewriting or touching the artifact; a changed Graph ID does not conflict with
the stable duplicate. The mail Door watches `mail/.door/summons/*.json`
without a content filter, so later JSONL appends create new immutable paths and
cannot hide or replay an older summon.

New Plant and Door state directories are created one level at a time and both
the new directory and its parent are fsynced before any fence, claim, lock, or
outcome is published. New Door publishers fsync lock owner metadata in a
temporary inode before an atomic hard link publishes the canonical lock name,
so their crashes leave an ignorable temp file or a complete lock. An
incomplete canonical lock may still belong to the shipped legacy publisher:
the Door boundedly rereads it, then fails closed without unlinking it. Such a
lock is removed only as an explicit offline migration after verifying no
legacy Door process exists. Complete locks whose recorded owner is dead are
reclaimed through the canonical lock inode itself. Each contender opens that
path under the canonical state root with no-follow and nonblocking semantics,
bounds and parses the regular file through the retained descriptor, and takes
an exclusive flock on that descriptor. While flock remains held, it rereads
and matches the observed PID/token through the same descriptor, revalidates
that the canonical pathname still names the retained device/inode, unlinks
only that canonical pathname, and fsyncs the retained canonical parent rather
than any original state-directory alias. The alias must still resolve to the
same root before acquisition continues. Contenders that retained the old inode
serialize; after one unlinks or publishes a successor, a delayed contender
observes a missing or changed pathname and cannot remove the successor.
Symlinks, FIFOs, non-regular nodes, oversize metadata, pathname replacement,
and intermediate state-root alias retargeting fail closed.

Door lock acquisition returns the `RootBoundDirectory` with the lease. The
retained directory descriptor and identity own every later state, lock, temp,
rename, unlink, and directory-fsync operation. State and lock publication
revalidate the configured alias immediately before visibility; an alias
retarget during claim save therefore fails closed before launch and cannot
write state into the successor root.

Shipped scalar `hwm` and v1 `(mtime,path)` states migrate under the Door lock
and the v2 replacement is durable before evaluation. A scalar `hwm` cannot
identify any processed paths at its high-water timestamp, so only that schema
advances to the immediately following representable timestamp; this preserves
never-fire-twice but can conservatively skip an unprocessed exact tie. A v1
cursor is authoritative through its known path, so migration keeps the same
timestamp with a durable `closedThroughPath` boundary and a separate seen set.
The boundary and seen paths survive claims at that timestamp and disappear
only after the frontier advances to a newer timestamp. An existing v1
in-progress claim retains its original Plant idempotency key until its durable
outcome is recovered. Negative zero, a scalar frontier with no finite successor,
and absolute or traversing legacy paths are rejected. Every constructed
migration passes through the canonical v2 parser before it can be saved or
used.
