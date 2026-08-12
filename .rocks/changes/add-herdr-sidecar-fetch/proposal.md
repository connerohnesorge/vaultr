# Change: Add Herdr sidecar fetch

## Why

Herdr topology sidecars now leave Git after their S3 copy is verified. Vaultr needs a narrow read verb that restores this evidence without widening Plant inventory walks.

## What Changes

- Add `vaultr session herdr <id>` for explicit Herdr topology inspection.
- Fetch `herdr.jsonl.zst` from the seal store only after a local miss.
- Materialize fetched sidecars as verified regular files.
- Keep capture and Plant inventory reads unchanged.

## Impact

- Affected specs: `session-seal-fetch`
- Affected code: `crates/vaultr/src/main.rs`, `crates/vaultr/src/seals.rs`, and Vaultr integration tests
