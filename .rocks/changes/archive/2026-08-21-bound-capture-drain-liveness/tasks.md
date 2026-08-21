# Tasks

## 1. Bounded response capture liveness

- [x] 1.1 Add a reset-on-chunk idle-timeout arm to the capture tee `tokio::select!` in `crates/plant/src/proxy.rs` that marks the stream torn (`complete=false`) and stops reading upstream when the inter-chunk gap exceeds a configurable idle bound
- [x] 1.2 Make the idle bound configurable with a 300s default and thread it into the proxy context
- [x] 1.3 Unit-test that a stalled upstream stream stages a `complete=false` Envelope within the idle bound and that a live stream emitting sub-bound chunks is never reaped

## 2. Periodic drain recovery

- [x] 2.1 Add a periodic `capture::recover_all` sweep in `crates/plant/src/main.rs` alongside the existing otel-flush loop, on a configurable interval
- [x] 2.2 Guard recovery synthesis so no reservation younger than the response capture idle bound is turned into an incomplete Envelope, so the sweep never races a live tee
- [x] 2.3 Unit-test that a session with a reaped head sequence and staged later completions drains fully after one sweep, and that a fresh live reservation is left untouched

## 3. Coverage validation

- [x] 3.1 Extend the capture coverage self-test / harness to assert in-window coverage stays at 100% across a simulated long session with one hung upstream stream mid-run
</content>
