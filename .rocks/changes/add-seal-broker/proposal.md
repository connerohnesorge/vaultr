# Change: Add the plant broker and its seal-push client

## Why

Seals left git on 2026-08-11, and with them the offsite copy a `git push` had
been providing every 30 minutes as a side effect. Since then a freshly sealed
capture exists on the Mac's local disk and nowhere else. The window has only
been closed by hand, twice, by the sessions doing the cutover.

The credential is the hard part, and it is what decides the shape. The seal
bucket is deliberately on the deny side of the agent-sandbox allow list —
agent-transcript data is classified regulated — and plant runs inside every
vended computer, i.e. inside every agent sandbox. So "give plant the write role"
silently hands every sandbox's machine identity reach into the store holding
every session transcript. A credential vendor minting short-lived scoped
credentials has the same defect with a shorter TTL.

A broker is the only shape where a sandbox can contribute a seal without its own
identity ever holding seal-bucket access.

## What Changes

- New `crates/broker` (`plant-broker`), the only holder of credentials reaching
  `s3://pantheon-vault-seals-athens`. Tenancy from tailnet identity; upload is
  idempotent and size-checked; per-tenant staleness is exported for scraping.
- New staged plant job `vault/jobs/staged/seal-push.30m.sh` in the dotfiles vault
  (not in this repo): enumerates local seals, reconciles against the broker,
  uploads the delta. Broker-unreachable is a failure, never a skip.
- No change to plant or vaultr. In particular plant does **not** acquire an AWS
  dependency, and the broker is not a plant subcommand, so the seal-bucket grant
  cannot follow plant onto a sandbox.

## Impact

- Affected specs: `seal-broker` (new capability)
- Affected code: `crates/broker/**` (new), `Cargo.toml` (workspace member)
- Deployment (athens, IRSA, staleness alert) is out of scope here — acropolis
  `#1221`. The job stays in `vault/jobs/staged/` until then.
