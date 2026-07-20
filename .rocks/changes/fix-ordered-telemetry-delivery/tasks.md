## 1. Ordered production ingestion

- [x] 1.1 Enable a 10-minute Prometheus out-of-order safety window and validate the rendered Prometheus CR.
- [x] 1.2 Add the persistent single-replica `alloy-otlp-gateway` Helm Application with the existing trusted-identity processors and OTLP exporters.
- [x] 1.3 Configure independent remote-write outputs to both stable Prometheus replica DNS names and persistent WAL storage.
- [x] 1.4 Render and policy-check both Alloy releases, then deploy the gateway without changing shared traffic.
- [x] 1.5 Prove direct gateway metrics, logs, and traces delivery before cutover.
- [x] 1.6 Switch the existing `alloy` Service selector to only the gateway and verify ready endpoints immediately.
- [x] 1.7 Remove the unused OTLP receiver and Service ports from the node-local Alloy DaemonSet after cutover proof.

## 2. Bounded Plant export

- [x] 2.1 Bound `cnb auth token` and OTLP HTTP attempts so a stalled telemetry dependency cannot block later flushes indefinitely.
- [x] 2.2 Export metrics and logs independently and retry transient transport, 429, and 5xx failures once.
- [x] 2.3 Retain bounded log records until `/v1/logs` acknowledges them without deleting records added during an in-flight export.
- [x] 2.4 Extend the Plant self-test to prove timeout/failure retention, recovery delivery, and acknowledged removal.
- [x] 2.5 Run formatting, focused telemetry checks, `cargo test --workspace`, and the release Plant self-test.

## 3. Production proof

- [x] 3.1 Deploy the updated Plant binary under launchd supervision and verify both proxy health endpoints.
- [x] 3.2 Observe at least 15 minutes of active traffic and prove continuous token counter samples in both Prometheus replicas.
- [x] 3.3 Prove zero new Prometheus out-of-order rejects, Alloy non-recoverable remote-write failures, and Plant telemetry stalls after the cutover.
