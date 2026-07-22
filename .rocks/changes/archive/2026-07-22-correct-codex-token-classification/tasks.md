# Tasks

## 1. Fix codex token classification

- [ ] 1.1 In `adapter.rs` `usage()` Codex branch, set `cache_read = cached_tokens`, `cache_creation = cache_write_tokens`, and `input = input_tokens.saturating_sub(cached_tokens).saturating_sub(cache_write_tokens)`.
- [ ] 1.2 Add a test using the real capture values (input_tokens=25296, cached_tokens=24320, cache_write_tokens=0) asserting `input + cache_read + cache_creation == input_tokens` and no double-count; keep a cache_write>0 case.

## 2. Verify

- [ ] 2.1 `cargo build -p plant` and `cargo test -p plant` pass.
