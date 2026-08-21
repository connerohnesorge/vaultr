## 1. Delete the duplicate

- [x] 1.1 Replace Plant telemetry's `adapter::parse_sse` import with `vaultr::recon::parse_sse` and delete the byte-identical Plant implementation.
- [x] 1.2 Add one focused parser check covering valid `data:` JSON, whitespace, blank data, `[DONE]`, non-data lines, and malformed JSON, then run `cargo test --workspace`.
