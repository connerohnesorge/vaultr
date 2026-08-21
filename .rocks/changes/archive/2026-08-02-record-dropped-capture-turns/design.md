# Record Dropped Capture Turns Design

## Context

Plant is the sole capture writer. It runs as a resident launchd process next to
the developer workload that competes for the same volume. Storage pressure is
therefore a normal operating condition, not an exceptional one.

The project constraint keeps Plant failure paths non-fatal where capture uptime
is at stake. That constraint is correct and stays. The defect is not that Plant
continues after a storage failure. The defect is that Plant continues silently
and leaves a capture that looks complete.

## Approach

Separate honesty from durability. Plant cannot guarantee that every Envelope
reaches disk when the volume is full. Plant can guarantee that a capture never
claims to be complete when it is not.

A gap record is the smallest artifact that carries that guarantee. It records
that a turn was observed and not captured. Downstream consumers gain a local
signal that does not depend on an external native transcript.

The headroom preflight then reduces how often the gap record is needed. It also
protects the gap record itself, because a failed multi-megabyte `state.json`
write can consume the free blocks the small marker requires.

## Placement

The gap record belongs in `.meta/<session-id>.json` rather than `turns.jsonl`.

`.meta` is already authoritative for Vaultr identity and discovery. It is
already rewritten on every successful turn by `update_meta`. It is small and
atomically replaced. Adding a counter and boundary timestamps there costs no new
file and no new lock.

Placing the marker in `turns.jsonl` was rejected. That file is the Envelope
generation and is sealed, scrubbed, and reconstructed. A non-Envelope record in
it would need handling in every reader.

## ADRs

### ADR-0003: Capture accounts for dropped turns durably and fails open

Plant preserves capture uptime on a storage failure, so a turn can be served and
not captured. Capture therefore records every such drop in the session `.meta`
entry before it abandons the turn.

The record is a count with first and last drop timestamps and the last error
reason. It is small enough to succeed under the storage pressure that caused the
drop. When the record itself cannot be written, Plant increments a process
counter that the health endpoint reports.

This accepts that a drop is not recoverable and rejects the alternative of
blocking the turn until capture succeeds. Blocking would trade a silent capture
gap for a stalled developer session, which the non-fatal constraint forbids.

The cost is that a capture can now be known-incomplete rather than assumed
complete. Consumers must treat a non-zero drop count as a correctness signal.
