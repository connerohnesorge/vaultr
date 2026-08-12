## Context

Vaultr already fetches `turns.jsonl.zst` for `session show`, `session path`, and `session fork`. The same S3 store now holds `herdr.jsonl.zst` under the vault-relative path.

Plant scans every Session Capture directory on recurring jobs. A fetch reachable from those scans would turn inventory into a bulk restore.

## Goals / Non-Goals

- Goal: Restore a Herdr topology sidecar during explicit inspection.
- Goal: Reuse the existing S3 transport and date-drift lookup.
- Goal: Preserve local-first behavior and atomic regular-file materialization.
- Non-goal: Fetch sidecars during Plant capture, sweep, or Cultivation Job inventory.
- Non-goal: Fetch a Session Capture when only Herdr topology is requested.

## Decisions

- Add `session herdr` as the explicit read boundary. The command streams decoded JSONL to standard output.
- Generalize the existing seal candidate and download path by seal filename. Do not add a second S3 implementation.
- Prefer local `herdr.jsonl` over local `herdr.jsonl.zst`. This preserves live append context.
- Treat S3 not-found and access denial as different loud failures.

## Risks / Trade-offs

- The command requires the AWS CLI on a remote miss. This matches existing Session Capture fetch behavior.
- A local raw sidecar can change while read. The command is inspection-only and does not alter Plant ownership.

## Migration Plan

No data rewrite is required. Existing local sidecars remain valid local hits.
