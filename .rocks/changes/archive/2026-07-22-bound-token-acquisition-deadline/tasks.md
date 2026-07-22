# Tasks

## 1. Decouple the token acquisition deadline

- [ ] 1.1 Add a `token_timeout: Duration` field to `Otel`, populated in `new()` from `VAULTR_OTEL_TOKEN_TIMEOUT_MS` (default 30s via a `DEFAULT_TOKEN_TIMEOUT` const), leaving the existing `timeout` (OTLP request deadline) unchanged.
- [ ] 1.2 In `flush()`, bound the `cnb auth token` subprocess with `self.token_timeout` instead of `self.timeout`; keep the OTLP `export` request on `self.timeout`.
- [ ] 1.3 Add a runnable check asserting `token_timeout` defaults to 30s and honors `VAULTR_OTEL_TOKEN_TIMEOUT_MS`, and that it is independent of `timeout`.

## 2. Verify

- [ ] 2.1 `cargo build -p plant` and `cargo test -p plant` pass.
