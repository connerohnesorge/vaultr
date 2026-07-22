# capture-telemetry — delta

## MODIFIED Requirements

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
