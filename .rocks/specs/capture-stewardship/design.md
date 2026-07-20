# Capture Stewardship Design

## ADRs

### ADR-0001: Capture owns preparation-ordered durable persistence and generation mutation

Capture reserves response sequence in preparation order because each request's
delta base advances at preparation time and Reconstruction applies persisted
records in file order. One strict per-session journal therefore owns both delta
advancement and sequence state. Completed responses are durably published to
private staging before success, then byte-exactly reconciled and drained in
sequence; startup recovery uses the same transaction. A slow earlier response
may block later draining, but cannot leave a later completion only in memory or
publish an unscrubbed stage in the Git-backed Capture tree.

The capture subsystem also owns the complete readiness, detachment, Sealing,
and recovery transaction through one retained no-follow session-directory
boundary and cooperative lock. Sweep may select inventory and policy, but MUST
NOT become a second generation mutator or filesystem owner. Scheduled
compression runs inside the process retaining both capture listeners; manual
compression must first retain both listeners and recover before it sweeps.
The scheduler's attempt fence is owned by `plant-agent-jobs/design.md`
ADR-0002; this decision owns only capture persistence and generation state.
