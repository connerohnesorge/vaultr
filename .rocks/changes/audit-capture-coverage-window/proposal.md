---
enables:
  - id: update-capture-persistence-order
    reason: the residual in-window loss this audit measures is repaired there
---

# Change: Measure capture completeness over Plant's observation window

## Why

A capture audit reported that Plant "under-captures" a long session:

```
claude 39548e28-5611-4c83-8ee6-021e17e9ff7c  native=235  captured=164  missing=82
```

Forensics on that exact session refute the framing. The audit's denominator is
every distinct assistant `requestId` in the harness transcript. That transcript
is a resumed session (`session_start_source: "resume"`, `original_start:
2026-07-17T19:18:51Z`), so it carries a full pre-resume history the current
Plant never fronted:

- **78 of 82 "missing" calls occurred at 18:23–19:18 on 07-17** — before Plant's
  first captured turn for this session at 19:18:51. During that same window Plant
  was demonstrably healthy: it captured ~15 other sessions (100+ Envelopes).
  So the early segment did not traverse the proxy at all (client started without
  the proxy in front, later resumed with it). The wire for those calls never
  reached Plant; it is not lost capture, it is out-of-scope traffic.
- **Within Plant's actual observation window (first capture forward), coverage is
  153 of 157 in-window `requestId`s ≈ 97.5%.** Only **4** genuine in-window
  misses remain, clustered at two fast-burst moments (07-18 17:15–17:16 and
  17:30) with no restart or panic anywhere near them (crash.log is quiet from
  07-18 04:14 to 07-19 16:40).

So the headline 35% loss is a **measurement artifact**: comparing a resumed
transcript's full history against a proxy that only fronted the tail. The small
real residual (in-flight Envelopes lost when concurrent streams complete around
process death or cancellation) is already owned by
`update-capture-persistence-order` — this change does not duplicate that repair.

The lasting fix is a coverage metric that Plant itself computes correctly, so
future audits and any stuck/reconcile signal use the right denominator instead
of manufacturing phantom loss from resume carryover.

## What Changes

- Add a read-only `plant sessions coverage <sid>` self-audit that compares
  captured Envelope `request-id`s against the harness transcript's assistant
  `requestId`s **restricted to Plant's observation window** — from the earliest
  captured `observed_at` (falling back to meta `original_start`) forward.
- Exclude pre-window transcript history explicitly, and flag resume carryover
  (`session_start_source == "resume"` with native `requestId`s predating the
  window) as out-of-scope rather than missing.
- Report in-window coverage as `captured / in_window_native` with the residual
  missing `request-id`s listed, so a real gap is visible and a resume artifact
  is labeled as such.

## Impact

- Affected specs: `capture-stewardship`
- Affected code: `crates/plant/src/sweep.rs` (coverage computation),
  `crates/plant/src/main.rs` (subcommand wiring), focused tests
- Related change: `update-capture-persistence-order` owns the in-window
  durability residual; this change only measures it, it does not repair it.
- No change to the capture write path — measurement is read-only over Session
  Captures and the transcript.
