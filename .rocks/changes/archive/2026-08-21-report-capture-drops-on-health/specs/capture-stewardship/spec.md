# Capture Stewardship Delta

## ADDED Requirements

### Requirement: Live capture status on the health endpoint

The Plant health endpoint MUST report whether capture is degraded. The endpoint
MUST report the recorded dropped-turn count, the measured storage headroom, and
the configured headroom floor. The endpoint MUST NOT report a healthy capture
while a drop is recorded. The `ok` field MUST keep its meaning as process
liveness.

#### Scenario: Capture is healthy

- WHEN no drop is recorded and measured headroom is at or above the floor
- THEN the health endpoint reports `capture_ok` as true
- AND the endpoint reports a recorded dropped-turn count of zero

#### Scenario: A drop is recorded in this process

- WHEN Plant records a dropped turn in `.meta`
- THEN the health endpoint reports `capture_ok` as false
- AND the endpoint reports the recorded dropped-turn count for this process

#### Scenario: A drop cannot be recorded

- WHEN Plant fails to write the `.meta` drop marker
- THEN the health endpoint reports `capture_ok` as false
- AND the endpoint keeps reporting the unrecorded dropped-turn count

#### Scenario: Headroom is below the floor

- WHEN measured free space on the vault volume is below the configured floor
- THEN the health endpoint reports `capture_ok` as false
- AND the endpoint reports the free space and the floor

#### Scenario: Headroom is above the floor

- WHEN measured free space on the vault volume is at or above the configured floor
- THEN the health endpoint reports the free space and the floor
- AND the endpoint reports `capture_ok` as true

#### Scenario: The volume cannot be measured

- WHEN the free-space query on the vault volume fails
- THEN the health endpoint reports a null storage headroom
- AND the endpoint does not report `capture_ok` as false for the measurement failure

#### Scenario: A selftest identifies the process

- WHEN the Plant identity selftest reads the health endpoint
- THEN the `ok` field reports process liveness
- AND a degraded capture does not change the `ok` field
