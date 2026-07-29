---
requires:
  - id: record-dropped-capture-turns
    reason: This change modifies the Storage headroom preflight requirement that proposal adds.
---

# Size the capture headroom floor to the capture write

## Problem

Plant under-captures long sessions. Six audited Session Captures show in-window
coverage between 64.6 percent and 88.8 percent. The audit excludes pre-proxy
resume carryover, so these are turns Plant observed and did not persist.

The cause is the storage headroom preflight in `prepare_capture`. The preflight
compares free space on the vault volume against a flat 2 GiB floor. A shortfall
records a dropped turn and returns before the journal write. The floor is not
related to the size of the write it protects.

Every audited gap gives the same drop reason:

| Session | Missing turns | Free space at last drop |
| --- | --- | --- |
| 2f431d1d-cbca-4695-93b2-675c3c4f06db | 30 | 1397743616 |
| 79ae7bce-a53f-4e42-a851-ed0b0ce81d5a | 30 | 1585754112 |
| 7c35aec5-d26e-4bca-b292-3669d44ead01 | 6 | 1901699072 |
| 8b6d1321-df0f-4107-bdf9-48e0ff3a2597 | 21 | 123387904 |
| d8fdfc19-c605-4812-b45e-86d6ce3adbd2 | 8 | 1755971584 |
| f1d20652-a5ff-4169-9545-dfbe2d42a8c1 | 23 | 1756278784 |

`~/.local/state/plant/launchd.log` holds 955 lines of the form
`capture failed: storage headroom N below floor 2147483648`. The largest logged
value is 2119376896, which is 2.02 GB of free space. The log has no panic, no
restart, and no reconnect near the drop window. Both health endpoints report
`ok` with `unrecorded_drops` of zero. The drain, the stage directory, and the
upstream connection are not involved.

The write the floor protects is small. The 40 most recent `state.json` files
measure 558 bytes at the minimum, 397965 bytes at the median, and 7448543 bytes
at the maximum. `atomic_replace` writes a temporary file beside the target and
renames it, so the peak demand is the old file plus the new file. For the
largest observed capture that peak is near 15 MB. The floor is more than 100
times that peak. Plant therefore refuses a 400 KB write with 2 GB free.

## Proposed change

Lower the default headroom floor from 2 GiB to 64 MiB.

64 MiB holds four times the peak demand of the largest observed capture. It
also holds the small `.meta` drop marker many times over, which is the failure
mode the preflight exists to prevent. The `PLANT_CAPTURE_HEADROOM_BYTES`
override stays unchanged.

Five of the six audited sessions dropped turns with more than 1.3 GB free. The
new floor captures all of those turns. The session at 123387904 bytes free was
under real storage pressure. That session stays partly protected, which is the
correct behavior.

## Out of scope

- Recovery of the already missing turns. The wire data is gone.
- The low storage headroom alert in the health job. That alert stays as is.
- Disk hygiene on the host.
