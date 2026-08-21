# Capture Stewardship Specification

## MODIFIED Requirements

### Requirement: Storage headroom preflight

Plant MUST measure free space on the vault volume before it reserves a sequence.
Plant MUST skip the large journal write when free space is below the configured
floor. The default floor MUST hold several times the peak demand of one capture
write. The default floor MUST NOT block a capture write while the volume holds
space for many such writes.

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
- THEN Plant applies a default floor of 64 MiB

#### Scenario: The volume holds space for many capture writes

- WHEN free space on the vault volume is 1 GiB
- AND no headroom floor is configured
- THEN Plant reserves the sequence and persists the journal as normal

#### Scenario: An operator configures the floor

- WHEN `PLANT_CAPTURE_HEADROOM_BYTES` holds a byte count
- THEN Plant applies that byte count as the headroom floor

#### Scenario: Free space cannot be measured

- WHEN the free-space query on the vault volume fails
- THEN Plant proceeds with the reservation
- AND Plant does not treat an unmeasurable volume as a full volume
