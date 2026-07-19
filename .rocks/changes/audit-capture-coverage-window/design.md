# Design: Observation-window coverage audit

## Root-cause analysis (session 39548e28)

Detection replay:

| Set | Count | Meaning |
|-----|------:|---------|
| Transcript assistant `requestId` (all) | 235 | audit denominator |
| Captured Envelope `request-id` | 164 | Plant's record |
| Transcript `requestId` with first-seen ≥ window start | 157 | in-window native |
| In-window native ∩ captured | 153 | correctly captured |
| **In-window native not captured** | **4** | genuine residual |
| Native predating window (resume carryover) | 78 | never fronted by Plant |

Window start = first captured `observed_at` = `2026-07-17T19:18:51Z`, which
matches meta `original_start` and `session_start_source: "resume"`.

Two independent facts kill the "capture stops after N calls" hypothesis:

1. The missing calls are the **earliest** ones (18:23–18:52), not a tail. A
   buffer/backpressure/flush failure would drop the *end* of a fast session, not
   the hour before Plant's first record.
2. Plant captured ~15 **other** sessions during that same 18:20–19:18 window, so
   it was up, binding both ports, and writing Envelopes. The early segment of
   this session simply did not go through `127.0.0.1:18923`.

The 4 real in-window misses are clustered (two moments, ~2.5%), with no exit,
panic, or restart adjacent. That signature matches concurrent in-flight streams
whose fire-and-forget `finish_capture` task did not land — the exact class of
loss that `update-capture-persistence-order` stages and reconciles. This change
does not re-solve it; it makes it *measurable* so the repair can be verified
against a correct baseline.

## Why measurement is the fix

There is no shipped Plant code computing native-vs-captured coverage — the audit
that raised the alarm was ad-hoc and used the whole transcript as denominator.
Shipping a proxy-window-aware coverage command means the correct denominator
lives in one place, so the next audit (and any future stuck/reconcile heuristic
that wants a completeness signal) cannot re-manufacture phantom loss from a
resumed transcript.

Deliberately **not** in scope (ponytail): no new daemon, no per-turn coverage
metric on the hot path, no automatic remediation. One read-only subcommand over
data already on disk.

## ADRs

### ADR-0001: Coverage denominator is Plant's observation window, not the full transcript

Plant only fronts wire traffic sent to its port. A resumed or re-pointed client
produces transcript history Plant never saw. Counting that history as "missing
capture" is a category error. Coverage MUST be computed over
`[window_start, ∞)` where `window_start` is the earliest captured `observed_at`
(fallback: meta `original_start`). Native `requestId`s whose first transcript
timestamp precedes `window_start` are reported as out-of-scope carryover, never
as loss. This is read-only and never mutates Session Captures.
