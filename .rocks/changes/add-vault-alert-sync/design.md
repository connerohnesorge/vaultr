# Design — Add Vault Alert Sync

Plant owns the sweep because it writes vault content (Vaultr stays read-only
against vault content). The extraction is deterministic and needs no agent
pane, so it follows the `Compress`/`Validate` inline-dispatch mold rather than
the Herdr agent lifecycle. If a user-facing `vaultr alerts` report is wanted
later, the parse can move into the vaultr crate then — plant → vaultr is the
allowed dependency direction.

## ADRs

### ADR-0001: Sweep captured mail instead of querying Grafana

Grafana (`grafana@cnb.rocks`) is the notice source, so an Alertmanager/Grafana
API pull was the alternative. Chosen: parse `vault/mail/**/messages.jsonl`.

- The full notice history (~1000 mails) is already on disk — the API would
  need backfill and pagination for the same data.
- No new credentials in the resident Plant process; capture uptime paths stay
  free of external auth failures.
- Accepted cost: tally freshness is bounded by MailSync cadence (manual
  today). The tally states its data horizon (latest swept mail date) so
  staleness is visible rather than silent.

### ADR-0002: Noise-list attribution, not per-notice cause analysis

"Self-caused" cannot be decided deterministically per notice. Chosen v1:
every OutOfSync notice counts as self-caused unless its app is listed in
`vault/alerts/noise-apps.txt` (curated, starts empty; candidates like
`efs-csi-driver` or CNPG-managed-role drift apps are Conner's call).

- Deterministic, testable, zero tokens; the per-app breakdown in `tally.md`
  makes any misattribution visible and correctable by editing one file.
- Upgrade path if the noise list proves too coarse: correlate notice
  timestamps with captured session activity per app (an agent job in the
  reflect mold). Recorded here so v1's ceiling is explicit.
