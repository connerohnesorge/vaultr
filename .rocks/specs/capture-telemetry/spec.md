# Capture Telemetry Specification

## Requirements

### Requirement: Shared permissive SSE event parsing

Plant telemetry and Vaultr reconstruction MUST use the same SSE event parser. The parser MUST accept line-oriented `data:` JSON payloads after trimming and MUST ignore blank payloads, `[DONE]`, non-data lines, and malformed JSON without failing the surrounding capture or reconstruction.

#### Scenario: Mixed valid and ignorable events

- WHEN an SSE response contains valid `data:` JSON mixed with blank data, `[DONE]`, non-data lines, and malformed JSON
- THEN telemetry and reconstruction receive the same ordered valid JSON events
- AND the ignorable content does not fail capture, telemetry, or reconstruction

### Requirement: Ordered production telemetry ingestion

The production OTLP path MUST preserve ordering for each cumulative Plant metric series before remote write. The shared OTLP Service MUST route application telemetry to one persistent active gateway, and that gateway MUST deliver a complete ordered stream to each Prometheus replica without distributing successive points among independent writers.

#### Scenario: Successive Plant counter points

- WHEN Plant exports successive cumulative counter points during active traffic
- THEN one persistent gateway processes those points in timestamp order
- AND each Prometheus replica receives the complete ordered series

#### Scenario: Gateway restart

- WHEN the telemetry gateway restarts with queued remote-write data
- THEN persistent WAL state survives the restart
- AND bounded out-of-order acceptance permits delayed replay without rejecting whole batches

#### Scenario: Reversible cutover

- WHEN the shared OTLP Service is moved from node-local collectors to the validated gateway
- THEN existing clients continue using the same endpoint name
- AND restoring the previous Service selector remains an immediate rollback

### Requirement: Bounded and acknowledged Plant telemetry export

Plant MUST bound token acquisition and OTLP network attempts, MUST prevent one
OTLP signal endpoint from blocking the other, and MUST retry transient failures
once. The token acquisition deadline MUST be independent of the per-request OTLP
deadline and large enough for a cold credential refresh to complete, so that
acquiring a fresh credential cannot be terminated mid-refresh and leave every
subsequent flush re-attempting an expired credential. Plant MUST retain
cumulative metric state and bounded log records across failed exports, and MUST
remove log records only after the logs endpoint acknowledges them.

#### Scenario: Telemetry dependency stalls

- WHEN token acquisition or an OTLP request exceeds its deadline
- THEN that attempt terminates without affecting Plant proxy traffic
- AND a later scheduled flush can proceed

#### Scenario: Expired credential is refreshed

- WHEN the cached credential has expired and refreshing it takes longer than a
  single OTLP request deadline but completes within the token acquisition
  deadline
- THEN the refresh completes and the flush uses the fresh credential
- AND subsequent flushes reuse the refreshed credential rather than re-attempting
  an expired one

#### Scenario: One signal endpoint fails

- WHEN metrics export fails while logs export succeeds, or logs export fails while metrics export succeeds
- THEN the successful endpoint is not blocked by the failed endpoint
- AND the failed signal remains eligible for a later flush

#### Scenario: Logs arrive during an export

- WHEN new request logs are recorded while an earlier snapshot is in flight
- THEN an acknowledgement removes only records included in that acknowledged snapshot
- AND newer records remain queued for a later flush

#### Scenario: Transient export failure recovers

- WHEN an OTLP endpoint returns a transient transport, 429, or 5xx failure and then recovers
- THEN Plant retries once within bounded time
- AND any still-unacknowledged data remains available to the next scheduled flush

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
