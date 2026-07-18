## ADDED Requirements

### Requirement: Shared permissive SSE event parsing

Plant telemetry and Vaultr reconstruction MUST use the same SSE event parser. The parser MUST accept line-oriented `data:` JSON payloads after trimming and MUST ignore blank payloads, `[DONE]`, non-data lines, and malformed JSON without failing the surrounding capture or reconstruction.

#### Scenario: Mixed valid and ignorable events

- WHEN an SSE response contains valid `data:` JSON mixed with blank data, `[DONE]`, non-data lines, and malformed JSON
- THEN telemetry and reconstruction receive the same ordered valid JSON events
- AND the ignorable content does not fail capture, telemetry, or reconstruction
