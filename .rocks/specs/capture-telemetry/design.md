## ADRs

### ADR-0001: One persistent writer owns each OTLP metric stream

The shared OTLP Service routes each cumulative metric stream through one persistent gateway instead of distributing successive points among independent node-local collectors. This trades a singleton ingestion boundary for deterministic ordering; client retries, persistent WAL state, stable StatefulSet identity, and an immediate Service-selector rollback bound that risk. Increasing the gateway replica count requires series-consistent routing and is not an ordinary scaling change.

### ADR-0002: Remote-write explicitly fans out to every Prometheus replica

The gateway targets stable Prometheus StatefulSet pod DNS names instead of a load-balanced receiver Service. Explicit fan-out gives every replica the complete ordered stream and leaves HA deduplication to Thanos; deployment validation must fail if an expected replica target is unavailable.
