## Context

Plant emits cumulative OTLP metrics and request logs every minute. The public OTLP auth proxy forwards them to `alloy.lgtm-stack.svc.cluster.local`. That Service currently selects every Alloy DaemonSet pod, so successive points for the same series enter different independent remote-write WALs and arrive at Prometheus out of order. The remote-write receiver rejects the whole batch on a non-recoverable 400.

The same Service is used by in-cluster OTLP clients. Fixing its backend transparently repairs the cluster-wide path without editing every workload or changing authenticated ingress.

Plant has a separate local failure mode: `cnb auth token` and both HTTP exports run serially without deadlines. Logs are removed from memory before `/v1/logs` succeeds. A stalled call delays every later flush; a failed call permanently loses those logs.

## Goals / Non-Goals

- Goals: one ordered metric writer, persistent retry state, complete Prometheus HA fan-out, bounded Plant exporter stalls, acknowledged log delivery, and a reversible production migration.
- Non-goals: changing request proxying, fixing provider 429 limits, changing dashboard PromQL, or solving workstation disk capacity.

## Decisions

### Infrastructure topology

A new `alloy-otlp-gateway` Helm release runs one StatefulSet replica with a persistent Alloy storage path. It contains only the existing OTLP identity processors and traces/metrics/logs exporters. Its Prometheus exporter forwards each ordered point to two remote-write components targeting the stable pod DNS names for Prometheus replicas 0 and 1. Each replica therefore receives a complete stream and Thanos can perform normal HA deduplication.

The node-local Alloy release remains a DaemonSet for file, journal, and node collection. Its chart-created Service is replaced by an Argo-managed `alloy` Service with the same ports and DNS name but a selector matching only the gateway. No client endpoint changes are required.

The gateway is created and validated through its own Service before the shared selector changes. A selector revert restores the prior path without rolling clients. After cutover validation, unused OTLP receivers and ports are removed from the node-local DaemonSet.

Prometheus receives a 10-minute `outOfOrderTimeWindow` before migration. This protects the cutover and bounded WAL replay but does not replace the single-writer topology.

### Plant exporter behavior

Token acquisition and each OTLP HTTP attempt receive short explicit deadlines. Metrics and logs export independently after authentication so one endpoint cannot block the other. Transient transport, 429, and 5xx failures retry once; other 4xx responses wait for the next scheduled flush.

Metrics remain cumulative in memory, so a later successful flush catches up naturally. Logs carry monotonic local IDs: a flush snapshots records through one ID and removes only those acknowledged by `/v1/logs`. New records and records retained across a failed flush cannot be removed accidentally. The existing 1,000-record outage bound remains.

## ADRs

### ADR-0001: One persistent writer owns each OTLP metric stream

The system will route the shared OTLP Service to one persistent gateway rather than load-balancing one cumulative series among node-local collectors. This accepts a brief gateway failover window in exchange for deterministic ordering. Client retries, persistent WAL state, stable StatefulSet identity, and an immediate Service-selector rollback bound that risk. Scaling the gateway beyond one replica requires series-consistent routing and is not allowed as an ordinary replica-count change.

### ADR-0002: Remote-write explicitly fans out to both Prometheus replicas

The gateway will target both Prometheus StatefulSet pod DNS names instead of the load-balanced Prometheus Service. A load-balanced receiver fragments one logical stream between HA replicas; explicit fan-out gives each replica complete data and leaves HA deduplication to Thanos.

## Risks / Trade-offs

- A singleton gateway is an ingestion availability boundary. Persistent WAL, stable stateful scheduling, producer retries, and conservative resources mitigate it; a future HA design must consistently hash complete series.
- Switching the shared Service affects all OTLP clients. The gateway is deployed and directly tested first, and the selector is the only cutover/rollback operation.
- Explicit Prometheus pod DNS depends on the current two-replica StatefulSet contract. Deployment validation MUST fail if either target is absent.
- A bounded Plant log buffer can still discard oldest logs during a prolonged outage; Session Captures remain the recovery source.

## Migration Plan

1. Enable the Prometheus out-of-order safety window and verify both replicas are ready.
2. Deploy the persistent gateway without changing existing traffic.
3. Send authenticated test metrics/logs through the gateway Service and prove both Prometheus replicas and Loki receive them.
4. Change the existing `alloy` Service selector to the gateway and confirm endpoints before and after Argo sync.
5. Observe active production traffic for at least 15 minutes: counters remain continuous, both Prometheus replicas advance, and no new out-of-order or non-recoverable remote-write failures occur.
6. Remove unused OTLP receivers from the node-local DaemonSet.
7. Roll back by restoring the previous Service selector if gateway health or delivery proof fails.

## Open Questions

None.
