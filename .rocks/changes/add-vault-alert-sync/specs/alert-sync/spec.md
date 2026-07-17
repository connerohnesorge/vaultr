# Alert Sync — Delta

## ADDED Requirements

### Requirement: ArgoCD notice extraction from captured mail

Plant MUST regenerate `vault/alerts/argocd-outofsync.jsonl` from the captured
inbox mail under `vault/mail/`, emitting one row per Grafana ArgoCD
application-sync notice (`from` = `grafana@cnb.rocks`, subject
`[Argo CD] Argo CD Application <OutOfSync|sync Unknown> — <app>`) with the
received timestamp, application name, and notice kind, deduplicated by
`internetMessageId`. The file is derived data rebuilt in full on every sweep.

#### Scenario: Grafana OutOfSync notice is captured

- WHEN a swept `messages.jsonl` contains an inbox mail from `grafana@cnb.rocks` with subject `[Argo CD] Argo CD Application OutOfSync — pages`
- THEN the regenerated index contains one row with that mail's received timestamp, app `pages`, and kind `OutOfSync`

#### Scenario: The same notice appears in multiple captures

- WHEN two swept mail records share an `internetMessageId`
- THEN the regenerated index contains exactly one row for that notice

#### Scenario: Unrelated mail is swept

- WHEN a mail record does not match the Grafana ArgoCD sender and subject shape
- THEN it contributes no row to the index

### Requirement: Monthly self-caused tally

Plant MUST render `vault/alerts/tally.md` from the regenerated index with,
per calendar month (UTC): the total notice count, a per-application breakdown,
and a self-caused count equal to the total minus notices for applications
listed in `vault/alerts/noise-apps.txt` (one app per line, missing file means
empty). Months whose self-caused count exceeds 30 MUST be flagged, and the
tally MUST state the received timestamp of the newest swept mail as its data
horizon.

#### Scenario: Noise-listed application is excluded

- WHEN `noise-apps.txt` lists `efs-csi-driver` and a month has 40 notices of which 12 are for `efs-csi-driver`
- THEN that month's tally shows total 40 and self-caused 28, not flagged

#### Scenario: Month over the bar

- WHEN a month's self-caused count exceeds 30
- THEN the tally flags that month against the ≤30 bar

### Requirement: Scheduled pure-Rust alert-sync job

Plant MUST run the sweep as an hourly `alert-sync` Cultivation Job dispatched
inline like `Compress` and `Validate` — no CLI, no model, no agent pane — and
MUST record the attempt outcome without aborting the scheduler when the sweep
fails.

#### Scenario: Scheduled sweep succeeds

- WHEN the `alert-sync` job fires and the mail tree is readable
- THEN both output files are regenerated and a `success` outcome is recorded

#### Scenario: Mail tree is missing

- WHEN the vault mail directory does not exist
- THEN the job records a non-success outcome and the scheduler continues unaffected
