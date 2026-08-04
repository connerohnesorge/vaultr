---
requires:
  - id: record-dropped-capture-turns
    reason: This change modifies the Storage headroom preflight requirement that proposal adds.
---

# Size the capture headroom floor to the capture write

## Problem

Plant under-captures long sessions. Audited Session Captures show in-window
coverage between 64.6 percent and 89.4 percent. The audit excludes pre-proxy
resume carryover, so these are turns Plant observed and did not persist.

The cause is the storage headroom preflight in `prepare_capture`. The preflight
compares free space on the vault volume against a flat 2 GiB floor. A shortfall
records a dropped turn and returns before the journal write. The floor is not
related to the size of the write it protects.

A full sweep of `vault/sessions/.meta` measures the blast radius. 123 Session
Captures record a dropped turn. Those Captures lost 2726 turns in total. 122 of
the 123 Captures give the headroom shortfall as the last drop reason. One
Capture gives a recovery conflict, which is a separate condition.

The loss continues to grow. An earlier sweep of the same directory found 104
affected Captures and 2319 lost turns.

The loss concentrates in long sessions. These are the largest:

| Session | Recorded dropped turns | Free space at last drop |
| --- | --- | --- |
| 019fae3f-684d-7770-a783-f248bd81d137 | 259 | 1658449920 |
| 019fae3f-697b-77c3-8885-4c4266e8324a | 243 | 1634832384 |
| 019fade5-7a58-78e1-a947-67555221d437 | 192 | 1661808640 |
| 019fae3f-67bb-7643-9d8f-56ed1df2116e | 144 | 1666461696 |
| 019fae67-f8f6-7a82-b00f-0e8a60992b98 | 137 | 1660628992 |
| a1943351-06cf-40ef-a998-a44cc6b275ca | 11 | 299966464 |
| 750bb71f-1d7c-4760-bbcf-2987ead0278d | 7 | 1748148224 |

The last two rows are audited Captures. The 80.7 percent Capture lost 11 turns,
which equals its recorded dropped-turn count. The 89.4 percent Capture records 7
dropped turns against 5 missing in-window request identifiers. The recorded drop
count covers the audit gap in both Captures. No audit gap remains without a
recorded drop, so no second defect is present.

The free space at the time of the drop shows the floor is the binding constraint.
The median free space across all refusals is 1565 MB. The value at the 90th
percentile is 1937 MB. 2217 of the 3096 refusals happened with more than 1 GiB
free.

Other causes are excluded. `~/.local/state/plant/launchd.log` holds 3096 lines of
the form `storage headroom N below floor 2147483648`. The log has no panic, no
restart, and no reconnect in the drop windows. For the audited Capture, drops
continue from 20:36:20 to 20:38:07 after the last successful observation. A
stopped proxy records nothing, so the proxy was alive and observing. The drain,
the stage directory, and the upstream connection are not involved.

The log holds one other drop path. 544 lines report `request has no session
identity`. Those lines end at log line 9497. The headroom refusals start at log
line 29411 and continue to the end of the log. The identity path is quiet through
the whole period of these refusals, so it is not a current cause.

The write the floor protects is small. A measurement of all 3466 persisted
`state.json` files gives 0.35 MB at the median, 2.89 MB at the 99th percentile,
and 9.03 MB at the maximum. `atomic_replace` writes a temporary file beside the
target and renames it, so the peak demand is the old file plus the new file. For
the largest observed capture that peak is near 18 MB. The floor is more than 100
times that peak. Plant therefore refuses a 350 KB write with 1.6 GB free.

## Proposed change

Lower the default headroom floor from 2 GiB to 64 MiB.

64 MiB holds more than three times the peak demand of the largest observed
capture. It also holds the small `.meta` drop marker many times over, which is
the failure mode the preflight exists to prevent. The
`PLANT_CAPTURE_HEADROOM_BYTES` override stays unchanged.

The full-population measurement confirms this value. The largest `state.json` in
the complete set is 9.03 MB against 7.45 MB in the earlier 40-file sample. 64 MiB
holds the peak demand of the larger maximum.

The recorded refusals prove the value is sufficient. The log holds 3096 headroom
refusals. The smallest free-space value at any refusal is 108498944 bytes. That
value is above 64 MiB. A 64 MiB floor therefore admits every one of the 3096
recorded refusals. The change recovers the full recorded loss.

## Residual risk

The host volume now holds 27 GB free of 461 GB. Refusals continued while the
volume held 1.5 GB to 2.0 GB free. The floor binds the capture, not the disk.

A volume under real pressure can still fail the write. The log holds `No space
left on device` errors from periods when free space reached zero. Under the new
floor those turns attempt the write and fail. Plant records the drop through the
same dropped-turn accounting, so the outcome is no worse than today. Host disk
capacity stays a separate concern.

Both health endpoints now report `unrecorded_drops` of 4. These are drops where
even the `.meta` marker write failed. Those turns leave no durable record. The
count was zero when this proposal was first written.

## Mitigation before this change ships

`PLANT_CAPTURE_HEADROOM_BYTES` already overrides the floor at runtime. An
operator can add that variable to `~/Library/LaunchAgents/com.cohnesor.plant.plist`
and reload the agent. This stops the loss without a code change. The variable is
not set on the host today.

## Out of scope

- Recovery of the already missing turns. The wire data is gone.
- The low storage headroom alert in the health job. That alert stays as is.
- Disk hygiene on the host.
