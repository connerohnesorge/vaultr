# capture-telemetry — delta

## ADDED Requirements

### Requirement: Harness-accurate token classification

Plant MUST classify each captured turn's tokens into non-overlapping buckets —
`input`, `output`, `cache_read`, `cache_creation` — such that
`input + cache_read + cache_creation` equals the turn's total input-side tokens,
regardless of harness. A token counted as cache read or cache creation MUST NOT
also be counted as `input`.

#### Scenario: Codex cached tokens are not double-counted

- WHEN a codex turn reports total input tokens that include a cached-token subset
  and a cache-write subset
- THEN `cache_read` is the cached subset and `cache_creation` is the cache-write subset
- AND `input` is the remaining uncached, non-written input
- AND the cached and cache-write tokens are excluded from `input`

#### Scenario: Claude classification is unchanged

- WHEN a claude turn reports input tokens that already exclude cache, plus
  separate cache-read and cache-creation counts
- THEN `input`, `cache_read`, and `cache_creation` are recorded as reported
- AND their sum equals the turn's total input-side tokens
