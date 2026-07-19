# Vault Doors — Delta

## ADDED Requirements

### Requirement: Door library owns the door routine

A shared TypeScript library in this workspace MUST provide a `door` entry
point taking a watch glob, an optional filter predicate, and a prompt builder,
and MUST own new-file detection, dedup fencing, watch-root policy, fire-rate
breaking, and agent launch — so an individual door job contains only its
watch, filter, and prompt.

#### Scenario: A door is a ten-line job

- WHEN a door job imports the library and calls `door` with watch, filter, and prompt
- THEN detection, fencing, policy, breaking, and launch behavior come from the library
- AND the door script contains no hand-rolled fencing or launch code

### Requirement: Batch firing with a high-water fence

A door MUST fire at most once per run: all files newer than the door's
persisted high-water mark that match the watch glob and pass the filter are
interpolated into a single prompt, and the mark MUST advance only after the
launch outcome is recorded so a given file never fires twice.

#### Scenario: A sync batch produces one session

- WHEN a sync lands 20 matching files before the door's next run
- THEN the door launches exactly one agent session whose prompt references all 20
- AND the high-water mark advances past them after the outcome is recorded

#### Scenario: Nothing new means no launch

- WHEN no matching file is newer than the high-water mark
- THEN the door exits without contacting Herdr

### Requirement: Ingestion-only watch roots

The library MUST enforce an allowlist of watchable ingestion roots — paths
written only by sync jobs — and MUST reject a door whose watch glob falls
outside it with a loud error before any launch, so a door cannot subscribe to
agent-written Vault Content.

#### Scenario: Watching agent-written content is rejected

- WHEN a door's watch glob targets a cultivation path such as learnings
- THEN the library refuses to evaluate the door and records the rejection
- AND no agent session is launched

### Requirement: Rolling-window fire breaker

The library MUST pause a door that exceeds the configured fires-per-window
limit, record the pause loudly in the door's ledger, and require a deliberate
manual re-arm before the door fires again.

#### Scenario: A runaway door is paused

- WHEN a door exceeds the fire limit within the rolling window
- THEN its next evaluation is skipped and the pause is recorded
- AND the door stays paused until manually re-armed

### Requirement: Typed launch over plant agent run

The library MUST launch agent sessions only through `plant agent run` and MUST
surface the lifecycle outcome as a typed
`Unavailable`/`Failed`/`Succeeded` result. It MUST NOT reimplement any part of
the Herdr lifecycle owned by Plant.

#### Scenario: Unavailable does not advance the fence

- WHEN `plant agent run` reports Herdr unavailable
- THEN the door's high-water mark does not advance
- AND the same files are eligible on the next run
