## ADDED Requirements

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

Plant MUST bound token acquisition and OTLP network attempts, MUST prevent one OTLP signal endpoint from blocking the other, and MUST retry transient failures once. Plant MUST retain cumulative metric state and bounded log records across failed exports, and MUST remove log records only after the logs endpoint acknowledges them.

#### Scenario: Telemetry dependency stalls

- WHEN token acquisition or an OTLP request exceeds its deadline
- THEN that attempt terminates without affecting Plant proxy traffic
- AND a later scheduled flush can proceed

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
