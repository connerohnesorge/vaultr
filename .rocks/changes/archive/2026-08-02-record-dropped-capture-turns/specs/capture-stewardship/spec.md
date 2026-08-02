# Capture Stewardship Delta

## ADDED Requirements

### Requirement: Durable dropped-turn accounting

Plant MUST record every observed turn that it fails to capture. Plant MUST NOT
leave a Session Capture that reports completeness it does not have.

#### Scenario: A journal write fails on a full volume

- WHEN `reserve` cannot persist `state.json` because the volume is full
- THEN Plant increments a dropped-turn count in `.meta/<session-id>.json`
- AND Plant records the first drop timestamp, the last drop timestamp, and the last error reason
- AND Plant serves the upstream response to the client without capture

#### Scenario: A completed Envelope cannot be staged or drained

- WHEN `finish_capture` fails after the response stream ends
- THEN Plant records the drop in the same `.meta` accounting fields
- AND the reserved sequence is reported as dropped rather than left as a silent absence

#### Scenario: The gap record itself cannot be written

- WHEN the volume cannot accept the small `.meta` replacement
- THEN Plant increments a process-lifetime dropped-turn counter held in memory
- AND the health endpoint reports that counter
- AND Plant serves the upstream response to the client without capture

#### Scenario: A capture with recorded drops is audited

- WHEN `plant sessions coverage` runs against a capture with a non-zero dropped-turn count
- THEN the report states the recorded drop count from `.meta`
- AND the report marks the capture as known-incomplete without a native transcript comparison

#### Scenario: A capture has no recorded drops

- WHEN no capture write has failed for the session
- THEN `.meta/<session-id>.json` carries a dropped-turn count of zero
- AND the coverage report does not mark the capture as known-incomplete

### Requirement: Storage headroom preflight

Plant MUST measure free space on the vault volume before it reserves a sequence.
Plant MUST skip the large journal write when free space is below the configured
floor.

#### Scenario: Free space is below the floor

- WHEN free space on the vault volume is below the configured headroom floor
- THEN Plant skips the `state.json` write for that turn
- AND Plant records the drop through durable dropped-turn accounting
- AND Plant serves the upstream response to the client without capture

#### Scenario: Free space is at or above the floor

- WHEN free space on the vault volume is at or above the configured headroom floor
- THEN Plant reserves the sequence and persists the journal as normal

#### Scenario: The floor is not configured

- WHEN no headroom floor is configured
- THEN Plant applies a default floor of 2 GiB

#### Scenario: Free space cannot be measured

- WHEN the free-space query on the vault volume fails
- THEN Plant proceeds with the reservation
- AND Plant does not treat an unmeasurable volume as a full volume

### Requirement: Low storage headroom alert

The health job MUST alert while captures are still complete. The alert MUST
report low headroom before capture writes begin to fail.

#### Scenario: Headroom approaches the floor

- WHEN the health job finds free space on the vault volume below the alert threshold
- THEN the job reports a low-headroom alert that names the free space and the floor

#### Scenario: A session recorded dropped turns

- WHEN the health job finds a session `.meta` entry with a non-zero dropped-turn count
- THEN the job reports a dropped-turn alert that names the session and the count
