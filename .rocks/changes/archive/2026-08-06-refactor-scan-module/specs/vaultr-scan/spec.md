## ADDED Requirements

### Requirement: Secret scan command behavior

The `vaultr scan` command SHALL preserve its current committed-text scanning, report, non-interactive, and local-review behavior after the module split.

#### Scenario: Clean scan

- WHEN the selected committed blobs contain no findings
- THEN the command prints the clean status
- AND the command exits with status 0

#### Scenario: JSON scan with findings

- WHEN `--json` is supplied and the selected committed blobs contain findings
- THEN the command prints the validate-compatible JSON report
- AND the command exits with status 1

#### Scenario: Non-interactive scan with findings

- WHEN review is disabled and the selected committed blobs contain findings
- THEN the command prints each finding
- AND the command exits with status 1

#### Scenario: Local review

- WHEN review is enabled and the selected committed blobs contain findings
- THEN the command serves the local review page
- AND the command re-scans committed blobs after review actions
