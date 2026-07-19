# Capture Stewardship Specification

## ADDED Requirements

### Requirement: Observation-window capture coverage audit

Plant MUST provide a read-only coverage audit for a single Session Capture that
compares captured Envelope response `request-id`s against the harness
transcript's distinct assistant `requestId`s, restricted to Plant's observation
window. The window start MUST be the earliest captured Envelope `observed_at`,
falling back to the Capture's meta `original_start` when no Envelope exists.
Native `requestId`s whose first transcript occurrence precedes the window start
MUST be reported as out-of-scope carryover (not as missing capture), and the
audit MUST report in-window coverage as captured over in-window native together
with the list of residual missing `request-id`s. The audit MUST NOT mutate any
Session Capture or transcript.

#### Scenario: Resumed session with pre-proxy history

- WHEN a resumed Session Capture's transcript contains assistant `requestId`s
  timestamped before the earliest captured Envelope's `observed_at`
- THEN those pre-window `requestId`s are reported as out-of-scope carryover
- AND in-window coverage counts only native `requestId`s at or after the window start

#### Scenario: Complete in-window capture

- WHEN every in-window native `requestId` has a matching captured Envelope `request-id`
- THEN coverage is reported as 100% with an empty residual missing list

#### Scenario: Genuine in-window gap

- WHEN an in-window native `requestId` has no matching captured Envelope `request-id`
- THEN it appears in the residual missing list and lowers the reported coverage
