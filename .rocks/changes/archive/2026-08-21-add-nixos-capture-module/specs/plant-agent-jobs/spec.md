## ADDED Requirements

### Requirement: Host-scoped and shared job discovery

Plant MUST treat flat jobs as host-scoped when `jobs/.hostname` exists.
Plant MUST compare the marker with the machine's short hostname.
Plant MUST load `jobs/shared/` on every hostname.
Plant MUST preserve flat job discovery when the marker is absent.
Manual job execution MUST use the same discovered job set.

#### Scenario: Marker matches

- GIVEN `jobs/.hostname` matches the machine's short hostname
- WHEN Plant scans jobs
- THEN Plant loads flat jobs
- AND Plant loads shared jobs

#### Scenario: Marker differs

- GIVEN `jobs/.hostname` differs from the machine's short hostname
- WHEN Plant scans jobs
- THEN Plant skips flat jobs
- AND Plant loads shared jobs

#### Scenario: Marker is absent

- GIVEN `jobs/.hostname` is absent
- WHEN Plant scans jobs
- THEN Plant loads flat jobs
- AND Plant loads shared jobs

#### Scenario: Local job is run manually elsewhere

- GIVEN `jobs/.hostname` differs from the machine's short hostname
- WHEN an operator manually runs a flat job by name
- THEN Plant rejects the unknown job without executing it
