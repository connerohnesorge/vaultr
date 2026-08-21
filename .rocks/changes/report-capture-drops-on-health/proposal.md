---
requires:
  - id: record-dropped-capture-turns
    reason: This change reports the dropped-turn accounting that proposal adds.
---

# Report capture drops on the Plant health endpoint

## Problem

A Plant health endpoint reports a healthy service while capture is fully
blacked out. An operator has no live signal that Session Captures are
incomplete.

At 16:04 on 2026-07-29 Plant recorded a dropped turn for a live session. Minutes
later both health endpoints answered:

```
{"service":"plant","ok":true,"harness":"claude-code","unrecorded_drops":0}
{"service":"plant","ok":true,"harness":"codex","unrecorded_drops":0}
```

The `unrecorded_drops` field counts only the drops that Plant failed to record
in `.meta`. A recorded drop is a successful write, so it leaves the counter at
zero. The endpoint therefore reports zero during the exact failure the counter
appears to describe.

The vault holds 2206 recorded dropped turns across 87 Session Captures. Two
recent captures show the shape of the loss:

| Session | Recorded drops | Free space at last drop |
| --- | --- | --- |
| 0e1cae3a-200d-4143-bf0a-736e1e3d9cd3 | 7 | 2144980992 |
| 26b92fc4-6640-4ddc-b190-9fd26f655739 | 24 | 818905088 |

The first capture missed the 2147483648 byte floor by 2502656 bytes and lost 7
turns. The host reported 46437068800 bytes free during this audit, so the
condition clears and returns without an operator action.

`plant sessions coverage` reports the recorded count correctly. That command
runs after the fact, one session at a time, and an operator runs it only after
a suspicion exists. `dropped_turns` has one reader in the code base, which is
`crates/plant/src/coverage.rs`. No live surface reports it.

Storage headroom is the leading indicator and is also absent from the endpoint.
An operator cannot see that free space approaches the floor until captures
already fail.

## Proposed change

Add three fields and one status flag to the health body in
`crates/plant/src/proxy.rs`.

- `recorded_drops` reports the count of drops this process recorded in `.meta`.
- `headroom_bytes` reports free space on the vault volume, or null when the
  volume cannot be measured.
- `headroom_floor` reports the configured floor.
- `capture_ok` reports false when a drop occurred or headroom is below the
  floor.

`ok` keeps its current meaning, which is process liveness. The Plant identity
selftest reads `ok` and must not change behavior.

`recorded_drops` counts in memory beside the existing `unrecorded_drops`
counter. The health endpoint performs no directory scan. One `statvfs` call
serves the headroom fields.

## Out of scope

- Recovery of the missing turns. The wire data is gone.
- The headroom floor value. `size-capture-headroom-floor-to-write` owns it.
- The health job alert. That job is a separate surface.
- Disk hygiene on the host.
