# Change: Make Plant telemetry delivery ordered and bounded

## Why

The Vaultr usage dashboard shows gaps while Plant is still successfully proxying requests. Production evidence from 2026-07-20 found the shared `alloy` Service distributing one cumulative metric stream across 39 independent Alloy remote-write WALs. Prometheus rejected out-of-order batches, including 108 non-recoverable batches and 11,556 failed samples during the investigated window. Plant can independently extend a gap because its serial telemetry loop has no timeout and discards log records before delivery is acknowledged.

## What Changes

- Add a dedicated single-writer, persistent Alloy OTLP gateway and route the existing `alloy` Service name to it, leaving node-local Alloy DaemonSets responsible only for node and file collection.
- Fan the gateway's ordered Prometheus stream to both Prometheus StatefulSet replicas through stable pod DNS rather than a load-balanced receiver Service.
- Enable a bounded Prometheus out-of-order acceptance window as migration and WAL-replay protection, not as the primary ordering mechanism.
- Bound Plant's auth and OTLP calls, retry transient failures once, export metrics and logs independently, and retain log records until the logs endpoint acknowledges them.
- Prove the fix in production by showing continuous Plant counters during active traffic and zero new out-of-order/non-recoverable remote-write failures after cutover.

## Impact

- Affected specs: `capture-telemetry`
- Affected code: `crates/plant/src/otel.rs`, focused Plant self-tests
- Affected infrastructure: `pantheon/eks-bootstrap` Alloy and Prometheus Helm values plus a dedicated Alloy gateway Argo CD Application
- Deployment: staged gateway creation and direct validation precede the existing `alloy` Service selector cutover; selector rollback remains immediate
- Unchanged: Plant's proxy/capture path, OTLP authentication, metric names and labels, Grafana dashboard queries, and application OTLP endpoint names
