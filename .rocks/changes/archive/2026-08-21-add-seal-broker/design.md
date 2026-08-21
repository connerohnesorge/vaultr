## Context

Seals were removed from git on 2026-08-11, closing the accidental offsite copy
that a 30-minute `git push` had been providing. `s3://pantheon-vault-seals-athens`
already exists as the store of record and `crates/vaultr/src/seals.rs` already
reads from it on a local miss. What is missing is the write leg.

The Mac has no durable machine credential for that bucket: its AWS access is SSO,
which terminates at an interactive `aws sso login`, and it is not a pod so it has
no IRSA. That fact alone would justify a credential vendor. Tenancy is what rules
that out — see ADR-0001.

## Goals / Non-Goals

- Goals: one holder of the seal-store grant; a client that carries no credential
  and no AWS tooling; idempotent size-checked uploads over the real corpus;
  server-side staleness detection.
- Non-Goals: a read route (the grant stays write-only until one exists); the
  athens deployment, IRSA role and Grafana alert (acropolis `#1221`); anything
  about `herdr.jsonl.zst`, which stays in git by a decision taken twice.

## Implementation Details

`crates/broker`, binary `plant-broker`, four modules:

| Module | Holds |
|---|---|
| `main.rs` | config, routing, body spooling, listing cache, drain |
| `tenant.rs` | tailnet range check, tailscaled whois, tenant cache |
| `store.rs` | the two `aws` CLI calls and their parsing |
| `seal.rs` | the key whitelist and the configurable seal-file set |
| `metrics.rs` | per-tenant ages and counters, Prometheus text |

Routes:

| Route | Auth | Purpose |
|---|---|---|
| `GET /healthz` | open | liveness |
| `GET /metrics` | open | Prometheus scrape |
| `GET /v1/seals` | tenant | `<key>\t<size>` per line, cached 5 min |
| `PUT /v1/seals/<key>` | tenant | store one seal, idempotently |

Configuration is environment only: `SEAL_BROKER_LISTEN`,
`SEAL_BROKER_METRICS_LISTEN`, `SEAL_BROKER_BUCKET`, `SEAL_BROKER_SPOOL`,
`SEAL_BROKER_SEAL_FILES`, `SEAL_BROKER_MAX_OBJECT_BYTES`,
`SEAL_BROKER_MAX_AWS_PROCESSES`, `SEAL_BROKER_TAILSCALE_SOCKET`,
`SEAL_BROKER_DEV_TENANT`.

## Decisions

- **`/healthz` and `/metrics` are open, everything under `/v1` is tenant-scoped.**
  Prometheus scrapes from inside the cluster and is not a tailnet peer; a metrics
  route behind tailnet identity would take staleness alerting dark, which is the
  one failure this design exists to catch. Neither open route discloses more than
  counts and ages. In athens, `/v1` and observability use separate listeners: the
  API binds only to the pod's tailnet IP, while a ClusterIP Service exposes an
  observability-only listener that returns 404 for every `/v1` path. Thus the
  scrape path does not accidentally become a cluster-internal broker path.
- **Bodies spool to disk before they are stored.** That buys an exact length to
  compare, a single-part upload needing no multipart permissions, and no ceiling
  below S3's own. The cost is disk in the pod: the largest seal on record is
  2.88 GB, so the spool volume is a deployment input, not an afterthought. The
  broker removes orphaned `seal-*.upload` files at process start: Kubernetes
  preserves an `emptyDir` across a container restart within the same pod, so the
  volume type alone does not prevent crash-loop accumulation.
- **The listing is cached for five minutes and dropped on every successful
  store.** Otherwise a client that just uploaded a seal is told on its next
  request that the seal is still missing.
- **Tenant state is in memory and does not survive a restart.** Safe in exactly
  one direction: a restart erases the series rather than resetting it to zero, so
  a tenant that was already dark stays caught by `absent()`. The alert must
  therefore be written against `absent()` as well as an age threshold.

## ADRs

### ADR-0001: A broker, not a credential vendor

The smaller design is a credential vendor: a tiny endpoint minting 15-minute
scoped S3 credentials so plant uploads directly to S3. It leaves the data path
unchanged, adds no single point of failure, and for one Mac it is the better
design.

It dies on tenancy. The seal bucket is deliberately on the deny side of the
agent-sandbox allow list — agent-transcript data is classified regulated — and
plant is installed on every vended computer, i.e. inside every agent sandbox. So
"plant holds the write credential" means every sandbox's machine identity gains
reach into the store holding every session transcript, silently reversing that
classification. A vendor has the same defect with a shorter TTL: it hands real
seal-bucket credentials to the box where untrusted agents run.

A broker is the only shape where a sandbox can contribute a seal without its own
identity ever holding seal-bucket access. **Consequence accepted on the record:**
a broker in the write path is a new single point of failure for durability. If it
is down, no tenant achieves an offsite copy. That is why the staleness export is
load-bearing rather than nice-to-have, and why the client must treat
broker-unreachable as a loud failure.

### ADR-0002: Tenancy from tailnet identity, not a token

Three candidates: a per-tenant shared token, a cnb OIDC bearer, or the tailnet.

A shared token reintroduces exactly what this service exists to remove — a
durable client secret living on the boxes we are keeping credentials off.

cnb OIDC has `offline_access`, so unattended refresh is possible in principle,
but the refresher is dead: `refresh-cnb-token`'s last ledger entry is
`2026-07-24T15:44:45Z, outcome: failed, "spawn: No such file or directory"`.
Depending on it means depending on a component whose failure mode is precisely
the silent stall this design is built to catch.

Tailnet identity holds no expiring secret on the client at all, and it is not a
new trust mechanism here: `cnb computer shell` already execs real `ssh` behind a
tailnet preflight, so tailnet identity already carries the most sensitive path in
this effort. Identity is read from the local `tailscaled`'s own view of the
netmap over its unix socket — no headscale API key, nothing to rotate.

**Consequence for deployment:** the broker's pod must see real tailnet source
addresses. A userspace-networking sidecar that proxies inbound connections
presents 127.0.0.1 as the peer and would erase every tenant, so the deployment
must prove kernel-mode TUN before anything else. `SEAL_BROKER_DEV_TENANT` is the
loopback-only escape hatch for local proving, announced at every startup.

### ADR-0003: The `aws` CLI, not the Rust SDK

`crates/vaultr/src/seals.rs` reads this same bucket through the `aws` CLI, and
the reasoning it records applies unchanged to the write leg: the hard part of
reaching this store is credential resolution, not the request. The broker must
resolve IRSA in athens, SSO on the Mac while it is being proven, and environment
variables in CI, and the CLI already resolves all three.

Nothing here is hot enough for process overhead to matter — one listing per
reconcile, one existence check and one upload per changed seal — and the body is
spooled to a file regardless, so the SDK's streaming would buy nothing. Adopting
`aws-sdk-s3` would add roughly forty crates to hold one PUT and one LIST, and
split the credential story across two mechanisms in one repository.

**Consequence:** the broker's container image must carry the `aws` CLI, which is
a heavier image than a static Rust binary. That is the trade accepted.

## Risks / Trade-offs

- Broker down means no tenant achieves an offsite copy → staleness alert is
  mandatory before the deployment is considered done, and the client fails loudly.
- Tailnet source addresses may not survive a userspace-networking sidecar →
  proven first in the deployment, ADR-0002.
- Spool disk for a 2.88 GB seal → sized explicitly in the deployment.
- In-memory tenant state does not survive a restart → the alert must include
  `absent()`.

## Migration Plan

1. This change: build and unit-test the broker, stage the client job.
2. Prove locally against the live backlog (acropolis `#1220`).
3. Deploy to athens with IRSA and the staleness alert, then `git mv` the job from
   `vault/jobs/staged/` into `vault/jobs/shared/` (acropolis `#1221`).

Rollback is deleting the crate and the staged job; nothing else changes
behaviour, because nothing calls the broker until step 3.

## Open Questions

- Whether `#1201`'s fetch-on-miss read side becomes a consumer of this broker
  rather than keeping its own read credential. Flagged there, not decided here.
