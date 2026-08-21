---
requires:
  - id: bound-capture-drain-liveness
    reason: This change modifies the periodic drain sweep that proposal introduces.
  - id: record-dropped-capture-turns
    reason: This change extends the drop accounting that proposal introduces.
---

# Isolate drain sweep failures

## Problem

Plant still under-captures long, fast Session Captures after the bounded
liveness fix and the drop accounting fix both shipped. An audited capture shows
the gap. Coverage counts only Plant's observation window, so pre-proxy resume
carryover is excluded.

| Session | in-window | captured | missing |
|---------|-----------|----------|---------|
| `d7d3c74b-aa87-4742-bf21-41e6d41cab21` | 163 | 146 (89.6%) | 17 |

The loss is silent. No Session Capture on this host records a dropped turn.
`grep -l '"dropped_turns": [1-9]' vault/sessions/.meta/*.json` matches zero
files. Every existing integrity check passes while turns are absent.

The periodic drain sweep that `bound-capture-drain-liveness` added is present in
the code and scheduled every 180 seconds. It is not running to completion. The
live daemon log records the failure.

```
[plant] drain sweep failed: capture commit: remove stage
/Users/cohnesor/.local/state/plant/capture-staging/a17f7163.../
f818e0b8-a8b9-40e1-8168-b69c961f593a/110-a88275f4-....json:
No such file or directory (os error 2)
```

## Root cause (proven)

The recovery transaction is one all-or-nothing batch across every Session
Capture. A single irreconcilable Session Capture disables backlog draining for
every other Session Capture on the host. Three separate conditions produce that
outcome, and two of them are benign.

**First, absent stage removal is fatal.** `commit_stage` in
`crates/plant/src/capture/persistence/commit.rs` ends with
`fs::remove_file(&stage.path)` and maps every error into a failure. An already
absent stage file is the exact intended end state. The live log line above is
this condition.

**Second, retired stage reconciliation is too narrow.** `commit_stage` accepts a
stage below `next_to_drain` only when `stage.sequence + 1 == next` and the
Envelope is the committed tail. A drain that persists several stages and then
stops before deleting them leaves orphans that can never satisfy that equality.
Session `b29c4c65-2e14-4a00-90bd-8e056ada249d` holds 24 such orphans at
sequences 321 through 344 while its journal reads `next_to_drain=8487`. Those
files have been present since 2026-07-25. They fail reconciliation on every
sweep and they cannot self-heal.

**Third, the batch propagates the first error.** `recover` in
`crates/plant/src/capture/persistence.rs` ends with
`for session in inventory { session.apply(live_cutoff)? }`. The `?` abandons
every later Session Capture. Global preconditions fail even earlier. A duplicate
session directory or a staged session without a journal returns before any
Session Capture is processed. The daemon log records both conditions.

Storage exhaustion created the initial damage. The log holds 132 instances of
`capture journal: persist ...: No space left on device (os error 28)` across
2026-07-21 through 2026-07-25. Session Captures now occupy 12 GB, and one
`turns.jsonl` is 2.2 GB. Free space has since recovered to 95 GB, so the
exhaustion is over. The inconsistent stage evidence it left behind is permanent
and keeps poisoning the sweep.

The resulting loss path is complete. A reservation whose response stream never
finishes blocks the drain head. Later completed Envelopes stage correctly and
wait. The sweep that exists to synthesize the dead reservation aborts on an
unrelated Session Capture before reaching this one. The session ends with its
staged Envelopes never appended to `turns.jsonl`. Nothing counts the loss,
because drop accounting covers only preparation and completion failures, not an
undrained backlog.

## Proposed fix

Make the sweep fault-isolating, idempotent, and honest.

Isolate each Session Capture. The sweep processes every Session Capture and
reports an aggregate failure at the end. One damaged Session Capture no longer
starves the others. Startup recovery keeps its strict fail-closed behavior,
because a fresh process must not serve traffic over unverified evidence.

Treat an absent stage file as success. The desired end state is an absent file.

Widen retired stage reconciliation. Accept any stage below `next_to_drain` whose
Envelope is already present in `turns.jsonl`, not only the immediately preceding
sequence.

Quarantine irreconcilable evidence. Move a stage that still conflicts into a
quarantine directory, record the loss through the existing drop accounting, and
let the drain continue. Evidence is preserved for inspection and never deleted.

Count undrained backlog. Report a stranded backlog on `/health` and record it as
a dropped turn, so an incomplete capture is never silent again.

## Out of scope

This change does not alter Envelope ordering, Sealing, or Reconstruction. It
does not reduce `state.json` write amplification. Storage growth is a real
contributing factor and belongs in a separate proposal.
