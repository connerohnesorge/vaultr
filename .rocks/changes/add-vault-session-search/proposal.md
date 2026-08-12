# Change: Add vault session search

## Why

Captured sessions contain decisions, tool output, and debugging evidence that
cannot be searched reliably. The current `session show` transcript omits tool
results. Grep cannot decode the captured wire format.

## What Changes

- Add `vaultr session index` and `vaultr session search`.
- Reconstruct the complete searchable conversation from captured envelopes.
- Store local derived Tantivy indexes for sessions and curated vault records.
- Add the Plant-owned periodic index update job.
- Document the `/Recall` and Herdr search integrations.

## Impact

- Affected code: `crates/vaultr`, `Cargo.toml`, and the Herdr plugin.
- Affected configuration: the dotfiles Plant job and Nix vaultr input.
- New specification: `vault-session-search`.
- Semantic search is not part of this change.
