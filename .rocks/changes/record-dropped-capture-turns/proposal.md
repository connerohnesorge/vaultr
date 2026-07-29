---
enables:
  - id: size-capture-headroom-floor-to-write
    reason: That proposal modifies the Storage headroom preflight requirement this proposal adds.
---

# Record dropped capture turns

## Problem

Plant under-captures Session Captures without leaving any evidence of the loss.
One audited capture shows the gap plainly. Coverage counts only Plant's
observation window, so pre-proxy resume carryover is excluded.

| Session | in-window | captured | missing |
|---------|-----------|----------|---------|
| `184748ce-e300-476f-8c35-a2d141e5c47b` | 17 | 10 (58.8%) | 7 |

This silently corrupts every downstream consumer, including learn passes,
Forks, and Reconstruction. The wire data for a missed call is gone after
Sealing, so the fix must make FUTURE captures complete or honest.

The distinguishing symptom is that the journal stays perfectly self-consistent.
`state.json` for the audited session reads `next_sequence=12`,
`next_to_drain=12`, and an empty `pending` map. `turns.jsonl` holds exactly 12
Envelopes. No stage file remains in `~/.local/state/plant/capture-staging`.
Every existing integrity check passes while 7 turns are absent.

## Root cause (proven)

Capture persistence fails open on a storage error and discards the turn.

`reserve` in `crates/plant/src/capture/persistence.rs` lines 472-487 loads the
journal from disk, advances the sequence in memory, and then calls `persist`.
`persist` at lines 123-146 writes `state.json` through
`crate::fsutil::atomic_replace`. That helper, in `crates/plant/src/fsutil.rs`
lines 5-17, creates a temporary file, writes it, and renames it over the target.

A write failure removes the temporary file and returns the error. The original
`state.json` is untouched. The `Journal` value is dropped, so the in-memory
sequence increment disappears. The next request loads the same on-disk state and
reserves the identical sequence number.

`serve` in `crates/plant/src/proxy.rs` lines 403-409 turns that error into one
`eprintln!` and sets `pending` to `None`. Lines 417-426 then stream the upstream
body to the client without capture. The turn is served correctly and lost
completely.

The result is a capture that is short by N turns with no sequence gap, no
pending entry, no stage file, and no durable marker. Loss is invisible to
everything except an external comparison against the native transcript.

### Live evidence

`~/.local/state/plant/launchd.log` records 828 capture failures. Two classes
dominate, and they differ in status.

| Failure text | Count | First line | Last line |
|---|---|---|---|
| `request has no session identity` | 544 | 11 | 9497 |
| `No space left on device (os error 28)` | 231 | 1579 | 24582 |

The log holds 24791 lines. The identity class stops at line 9497 because the
`count_tokens` exact-path fix already landed, as recorded in
`crates/plant/src/adapter.rs` lines 68-73. That class is closed.

The storage class runs to line 24582 and remains open. The vault volume reports
96 percent use with 18 GiB available. Storage pressure on this host is a
recurring condition driven by Rust and Go build caches.

The audited session ran on 2026-07-25 from 16:36 to 18:28, inside the window
where the storage class is active.

## Relationship to `bound-capture-drain-liveness`

That proposal fixes a different loss path. It addresses a zombie stream that
reserves a sequence and never stages it, which head-of-line-blocks the drain and
strands later Envelopes in staging.

The two causes are distinguishable on disk. A drain block leaves
`next_to_drain` far below `next_sequence` and leaves stage files present. This
change addresses the case where both counters agree and staging is empty.

Both changes are needed. Neither one subsumes the other.

## Fix

Three changes to the `capture-stewardship` capability.

First, account for every dropped turn durably. A capture write failure must
append a compact gap record to the session `.meta` entry. A gap record is
orders of magnitude smaller than the Envelope, so it is far more likely to
succeed under storage pressure. When even the gap record cannot be written,
Plant increments a process counter and reports it on the health endpoint.

Second, check storage headroom before the large write. Plant measures free
space on the vault volume before it reserves a sequence. Below the configured
floor Plant skips the multi-megabyte `state.json` write and records the gap
directly. This keeps the last free blocks available for the small marker and
stops the repeated failure storm.

Third, alert before capture degrades. The health job gains a low-headroom
alert, so the operator sees the condition while captures are still complete.

Capture uptime is preserved. Plant continues to serve the turn on every failure
path, per the project constraint that Plant failure paths stay non-fatal.

## Open question for review

The proposal sets the default headroom floor at 2 GiB and makes it
configurable. The request did not state a value. Confirm or change the default
before accept.
