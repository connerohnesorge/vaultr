## Implementation Details

Use three typed layers:

- `AgentRunTarget` stores the immutable Herdr workspace and pane identifiers.
- `AgentRunPaneIdentity` distinguishes a terminal-only identity from a
  session-bound identity.
- `AgentRunCheckpoint` stores a tagged lifecycle phase and only the identity
  required by that phase.

The checkpoint phases are `WorkspaceCreated`, `Launched`, `Ready`,
`Submitting`, `Working`, `TerminalObserved`, and `Captured`. The `Captured`
phase requires a session-bound pane identity. Earlier phases can carry a
terminal-only identity when Codex has not exposed its session identifier.

Persistence will reject a target change, terminal replacement, or session
replacement. It will accept only forward phase transitions and a valid
terminal-only to session-bound enrichment.

Existing flat in-progress receipt objects will remain readable. The decoder
will map them into the nearest safe tagged checkpoint. A flat object that lacks
the fields required for a safe recovery checkpoint will remain pending and
will use the existing operator recovery path.

## Context

Herdr exposes terminal and agent-session identity at different points in the
lifecycle. Codex can expose its session identifier only after the run. Durable
recovery must preserve this observation boundary without allowing arbitrary
stage and field combinations.

## Goals / Non-Goals

- Goals: make checkpoint invariants visible in Rust types
- Goals: preserve fail-closed recovery behavior
- Goals: preserve compatible reading of pending receipts
- Non-Goals: change Herdr lifecycle timing
- Non-Goals: change capture matching rules
- Non-Goals: change scheduled job cadence

## Decisions

- Decision: use tagged checkpoints with a separate immutable target
- Decision: model session discovery as identity enrichment
- Decision: keep live Herdr revalidation at external-state boundaries

## ADRs

### ADR-0001: Represent durable lifecycle checkpoints as tagged states

The durable record must distinguish an early workspace checkpoint, a live pane
checkpoint, a terminal observation, and a captured session. A flat structure
cannot express which fields are required by each phase. Tagged checkpoints
make invalid combinations unrepresentable while keeping external Herdr
revalidation necessary at each observation boundary.

## Risks / Trade-offs

- Existing receipts need compatibility decoding.
- The checkpoint enum adds serialization code.
- Explicit variants increase test fixtures.

## Migration Plan

Read existing flat receipts through a compatibility decoder. Write new
checkpoints in the tagged representation. Retain receipts that cannot be
mapped to a safe recovery state.

## Open Questions

- None.
