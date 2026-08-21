## 1. Broker

- [x] 1.1 Add `crates/broker` to the workspace
- [x] 1.2 Store access through the `aws` CLI: key-scoped listing, full listing,
      single-part put
- [x] 1.3 Tenant resolution: tailnet range check, tailscaled whois over the unix
      socket, per-peer cache, loopback dev tenant announced at startup
- [x] 1.4 Key whitelist with a configurable seal-file set
- [x] 1.5 Routes, body spooling, listing cache, graceful drain
- [x] 1.6 Per-tenant contact/upload ages and counters in Prometheus text

## 2. Client

- [x] 2.1 `vault/jobs/staged/seal-push.30m.sh` — reconcile and upload the delta
- [x] 2.2 Document `staged/` in `vault/jobs/AGENTS.md` as a non-bucket

## 3. Proof

- [x] 3.1 Unit and served-surface tests (22)
- [x] 3.2 Listing verified against the live store: 9,421 objects,
      7,458,175,928 B
- [x] 3.3 Reconcile verified against ground truth: 9,573 local, delta 159
      (152 new keys, 7 size changes, 40.4 MB), both oversized seals evaluated
      and correctly not in the delta
- [x] 3.4 Idempotence proven end to end against the production store with no
      write: a matching seal returns `unchanged`
- [x] 3.5 Default configuration refuses a loopback caller (403) while `/healthz`
      and `/metrics` stay open

## 4. Not in this change

These are not tasks of this change and never were. They are checkboxes only by
accident, which held the change at 13/15 and blocked its archive. Both shipped
under their own tickets:

- Full backlog upload run — acropolis `#1220`, closed.
- athens deployment, IRSA role, Grafana staleness alert, and activating the job
  into `shared/` — acropolis `#1221`, closed. The deployment runs in the
  `plant-broker` namespace, the ServiceAccount carries
  `arn:aws:iam::485494358268:role/plant-broker-athens`, and `seal-push.30m.sh`
  has moved from `staged/` into `shared/`. No PrometheusRule matches the
  staleness alert; the ServiceMonitor scrapes the broker's per-tenant upload-age
  metrics, so whether an alert was ever built on them is worth confirming
  against `#1221` rather than assumed from this list.
