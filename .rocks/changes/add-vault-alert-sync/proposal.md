# Add Vault Alert Sync

## Why

Height 1 ("Athens stays in sync", ≤30 self-caused ArgoCD out-of-sync notices
per month) has no automated measurement — the named challenge blocking it.
The notices already land in the vault: Grafana mails
(`grafana@cnb.rocks`, subject `[Argo CD] Argo CD Application OutOfSync — <app>`)
are captured by MailSync into `vault/mail/YYYY/MM/DD/messages.jsonl` (~1000
historical notices today). Nothing extracts them or tallies the monthly metric.

## What Changes

- New pure-Rust Cultivation Job `alert-sync` (`Kind::AlertSync`, hourly, no
  agent pane — dispatched inline in `run_job` like `Compress`/`Validate`).
- Sweep scans captured inbox mail for Grafana ArgoCD notices and regenerates
  `vault/alerts/argocd-outofsync.jsonl` (one row per notice: ts, app, kind),
  deduped by `internetMessageId`. Derived data, rebuilt from scratch each run —
  no incremental state.
- Renders `vault/alerts/tally.md`: per-month totals, per-app breakdown,
  self-caused count (total minus apps listed in curated
  `vault/alerts/noise-apps.txt`), months over the ≤30 bar flagged.
- Tally freshness is bounded by MailSync cadence (currently manual); the sweep
  reads only what MailSync has captured. Scheduling MailSync is out of scope.

## Impact

- Affected specs: alert-sync (new capability)
- Affected code: `crates/plant/src/jobs.rs`, new `crates/plant/src/alerts.rs`
- Dotfiles-side (one-liner, out of this repo): vault `.gitignore` for the two
  derived files; `noise-apps.txt` stays tracked.
