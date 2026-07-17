# Tasks

## 1. Alert sweep

- [ ] 1.1 Add `crates/plant/src/alerts.rs`: scan `vault/mail/**/messages.jsonl` inbox records for `grafana@cnb.rocks` subjects `[Argo CD] Argo CD Application <OutOfSync|sync Unknown> — <app>`, dedupe by `internetMessageId`, regenerate `vault/alerts/argocd-outofsync.jsonl` (ts, app, kind) sorted by ts
- [ ] 1.2 Render `vault/alerts/tally.md`: per-month (UTC) totals + per-app breakdown + self-caused (total minus apps in `vault/alerts/noise-apps.txt`), flag months with self-caused > 30, state the data horizon (newest swept mail ts)

## 2. Scheduling

- [ ] 2.1 Add `Kind::AlertSync` and an hourly `alert-sync` job entry in `jobs.rs`, dispatched inline in `run_job` like `Compress`/`Validate` (no cli/model), non-fatal on sweep failure
- [ ] 2.2 Dotfiles-side one-liner: gitignore `vault/alerts/argocd-outofsync.jsonl` and `vault/alerts/tally.md`; keep `noise-apps.txt` tracked

## 3. Validation

- [ ] 3.1 Unit tests: subject parsing (both notice kinds, em-dash, app names with spaces/dashes), internetMessageId dedupe, tally math with noise exclusion and over-bar flag, missing noise file
- [ ] 3.2 `cargo test --workspace` green, then run one live sweep and check `tally.md` per-app counts against known mail volumes (e.g. `pages` ≈ 262 historical notices)
