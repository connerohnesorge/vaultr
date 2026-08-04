# Bound capture drain liveness

## Problem

Plant under-captures long, fast Session Captures: only the Envelopes before the
first stalled sequence reach `turns.jsonl`; everything after is stranded. Two
audited captures show it plainly (in-window coverage, carryover excluded):

| Session | in-window | captured | missing |
|---------|-----------|----------|---------|
| `32d1e59c-…` | 102 | 55 (54%) | 47 |
| `cc05e3c7-…` | 146 | 51 (35%) | 95 |

This silently corrupts every downstream consumer (learn passes, Forks,
Reconstruction). The wire data for missed calls is gone once the capture is
sealed, so the fix must make FUTURE captures complete.

## Root cause (proven)

Envelope persistence is strictly **preparation-ordered**: `drain()` writes
staged Envelopes into `turns.jsonl` only in `next_to_drain` order and stops at
the first sequence not yet staged (`crates/plant/src/capture/persistence.rs`
`drain`, lines 457-470). Ordering is structural — each request's delta base
advances at preparation time, so out-of-order persistence would corrupt
Reconstruction (see `capture-stewardship` ADR-0001).

The response-capture tee (`crates/plant/src/proxy.rs`, lines 414-470) stages an
Envelope for **every** clean or torn stream end. The one exit that does NOT
stage is a stream that never ends: its `tokio::select!` (lines 417-425) has only
two arms — `tx.closed()` and `upstream_stream.next()` — and **no idle-timeout
arm**. When an upstream connection breaks without a clean close (the repeated
`serve error: connection error` in `~/.local/state/plant/launchd.log`) and the
client has already moved on via a retry, that tee task hangs forever in
`upstream_stream.next()`. Its sequence is reserved in the journal but never
staged.

That single zombie sequence head-of-line-blocks the whole session. Recovery
(`recover_all`) synthesizes such a dead reservation as `response.complete=false`
and drains the backlog — but it runs **only at process startup**
(`crates/plant/src/main.rs:436`). A long-lived session never restarts Plant, so
the backlog is stranded for the session's entire life and lost at Sealing.

### Live evidence

Both audited sessions have **exactly one** missing sequence at the drain head,
with every later Envelope fully captured and staged behind it:

```
32d1e59c  next_sequence=104  next_to_drain=56   staged on disk: seq 57..103 (47)   seq 56: NOT staged
cc05e3c7  next_sequence=153  next_to_drain=57   staged on disk: seq 57..152 (95)   seq 57: NOT staged
```

Seq 56 was reserved at `00:20:43`; reservations kept flowing for 20 more minutes
(seq 103 at `00:40:24`), all staged, none drained. `142` such orphaned stage
files sit in `~/.local/state/plant/capture-staging` right now, waiting for a
restart that a live session will not trigger.

## Fix

Two changes to the `capture-stewardship` capability, both in the resident
runtime, no schema or on-disk format change:

1. **Bound the tee's liveness.** Add an idle-timeout arm to the capture
   `tokio::select!`: if no upstream byte arrives for a configurable interval
   (default well above Anthropic/Codex streaming ping cadence and any thinking
   pause, e.g. 300s), finalize the Envelope as `complete=false`, drop the
   upstream, and stage it. This eliminates the zombie at the source, so a broken
   upstream can no longer strand a session.

2. **Run recovery on a timer, not only at startup.** Add a periodic in-process
   drain-recovery sweep (mirroring the existing 60s otel-flush loop in
   `main.rs`) that drains any stranded backlog on a live Session Capture within
   a bounded interval. It reuses the existing `recover_all` transaction, guarded
   so a genuinely in-flight reservation (younger than the tee idle bound) is
   never prematurely synthesized. This is defense-in-depth: it also drains
   backlogs from any future non-staging cause (task panic, transient disk-full),
   not just the zombie.

Fix (1) alone closes the observed failure; fix (2) makes (1)'s synthesized gaps
drain promptly and bounds any residual stall. `podGC`-style deploy is Conner's;
this proposal is diagnosis + plan only.

## Out of scope

- The `No space left on device` capture failure in the log is a separate,
  environmental issue (the Data volume is at 97%). A request whose
  `prepare_capture` fails at request time is never reserved and cannot be
  recovered by drain — worth hardening later (retry/backoff, disk-pressure
  surfacing), but it is intermittent and not the 35-54% systemic gap.
</content>
</invoke>
