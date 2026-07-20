# Capture Stewardship Specification

## ADDED Requirements

### Requirement: Observation-window capture coverage audit

Plant MUST provide a read-only coverage audit for a single Session Capture that
compares captured Envelope response `request-id`s against the harness
transcript's distinct assistant `requestId`s, restricted to Plant's observation
window. The window start MUST be the earliest captured Envelope `observed_at`,
falling back to the Capture's meta `original_start` when no Envelope exists.
The audit MUST stream every sealed-then-live-raw Envelope generation in
chronological generation order using Reconstruction's canonical traversal,
including complete concatenated records and its live-tail/error semantics, no
matter which canonical sibling is selected. It MUST stream the native
transcript and retain memory bounded by the largest record plus comparison ID
sets. Capture evidence that cannot be opened, decoded, or parsed under
Reconstruction's rules MUST fail the audit.
Native `requestId`s whose first transcript occurrence precedes the window start
MUST be reported as out-of-scope carryover (not as missing capture), and the
audit MUST report in-window coverage as captured over in-window native together
with the list of residual missing `request-id`s. Harness support MUST be derived
from Envelope truth. Codex Captures and Claude windows with zero comparable
native IDs MUST fail explicitly and MUST NOT print a percentage. The audit MUST
NOT mutate any Session Capture or transcript.

#### Scenario: Resumed session with pre-proxy history

- WHEN a resumed Session Capture's transcript contains assistant `requestId`s
  timestamped before the earliest captured Envelope's `observed_at`
- THEN those pre-window `requestId`s are reported as out-of-scope carryover
- AND in-window coverage counts only native `requestId`s at or after the window start

#### Scenario: Complete in-window capture

- WHEN every in-window native `requestId` has a matching captured Envelope `request-id`
- THEN coverage is reported as 100% with an empty residual missing list

#### Scenario: Resumed capture has sealed and live raw generations

- WHEN a Capture has a sealed generation followed by a live raw generation
- THEN both generations contribute in that order regardless of which canonical sibling is selected
- AND complete concatenated records and the final live tail follow Reconstruction's behavior

#### Scenario: Capture evidence is malformed or unreadable

- WHEN any selected Capture generation cannot be opened, decoded, or parsed under Reconstruction's rules
- THEN the audit fails instead of omitting that evidence

#### Scenario: Large capture and transcript

- WHEN release coverage audits hold record size and ID cardinality constant while
  decoded Capture evidence scales through at least 919 MiB
- THEN it streams both inputs with memory bounded by the largest record plus comparison ID sets

#### Scenario: Genuine in-window gap

- WHEN an in-window native `requestId` has no matching captured Envelope `request-id`
- THEN it appears in the residual missing list and lowers the reported coverage

#### Scenario: Codex has no comparable native denominator

- WHEN Envelope truth identifies the Capture as Codex
- THEN the audit fails with explicit unsupported or no-comparable-ID text
- AND it does not print a percentage

#### Scenario: Claude has zero comparable native IDs

- WHEN a Claude transcript contains no comparable assistant `requestId`
- THEN the audit fails with explicit no-comparable-ID text
- AND it does not report `0/0` as 100%
