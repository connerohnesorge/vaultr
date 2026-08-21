# Capture Stewardship Specification

## ADDED Requirements

### Requirement: Observation-window capture coverage audit

Plant MUST provide a read-only coverage audit for a single Session Capture that
compares captured Envelope response `request-id`s against the harness
transcript's distinct assistant `requestId`s, restricted to Plant's observation
window. The window start MUST be the earliest captured Envelope `observed_at`,
falling back to the Capture's meta `original_start` when no Envelope exists.
The audit MUST stream every retained sealed, detached, and live raw Envelope
generation in chronological generation order using Reconstruction's canonical
traversal, including complete concatenated records and its live-tail/error
semantics, no matter which canonical sibling is selected. Detached evidence
MUST contribute after the sealed base unless the same retained sealed handle
proves that the suffix from the detached generation's recorded base through
that handle's captured length decodes to the detached raw digest; only that
proof MAY omit the detached handle as already committed. It MUST stream the
native transcript and retain memory bounded by the largest record plus
comparison ID sets. Capture evidence that cannot be opened, decoded, or parsed
under Reconstruction's rules MUST fail the audit.
Native `requestId`s whose first transcript occurrence precedes the window start
MUST be reported as out-of-scope carryover (not as missing capture), and the
audit MUST report in-window coverage as captured over in-window native together
with the list of residual missing `request-id`s. Harness support MUST be derived
from Envelope truth. Codex Captures and Claude windows with zero in-window
comparable native IDs, including nonempty all-carryover transcripts, MUST fail
explicitly and MUST NOT print a percentage. The audit MUST NOT mutate any
Session Capture or transcript.

#### Scenario: Resumed session with pre-proxy history

- WHEN a resumed Session Capture's transcript contains assistant `requestId`s
  timestamped before the earliest captured Envelope's `observed_at`
- THEN those pre-window `requestId`s are reported as out-of-scope carryover
- AND in-window coverage counts only native `requestId`s at or after the window start

#### Scenario: Complete in-window capture

- WHEN every in-window native `requestId` has a matching captured Envelope `request-id`
- THEN coverage is reported as 100% with an empty residual missing list

#### Scenario: Resumed capture has sealed, detached, and live raw generations

- WHEN a Capture has a sealed base, an unproven detached generation, and a newer live raw generation
- THEN all three generations contribute in that order regardless of which canonical sibling is selected
- AND complete concatenated records and the final live tail follow Reconstruction's behavior

#### Scenario: Detached generation is already committed

- WHEN the same retained sealed handle proves that its decoded suffix from the detached base has the detached raw digest
- THEN the committed suffix contributes through the sealed handle
- AND the detached handle is not visited a second time

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

#### Scenario: Claude has zero in-window comparable native IDs

- WHEN a Claude transcript contains no comparable assistant `requestId` at or
  after the observation-window start, including when every ID is carryover
- THEN the audit fails with explicit no-comparable-ID text
- AND it does not print a percentage
