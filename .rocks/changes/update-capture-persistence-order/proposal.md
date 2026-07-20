# Change: Preserve Session Capture persistence order

## Why

Plant advances request-history deltas during preparation but appends Envelopes
when independently streamed responses finish. Real Session Captures contain
completion-order inversions and one terminated record with two concatenated
Envelopes, proving that concurrent completion can both violate delta lineage and
interleave JSONL writes.

The repair needs more than an append mutex: completed later responses must
survive while an earlier preparation sequence is still open, and a Plant restart
must reconcile that durable backlog before new capture or Sealing begins.

## What Changes

- Reserve a private per-session preparation sequence atomically with request
  delta-base advancement.
- Stage completed Envelopes in Plant's private state tree and drain them into
  the Session Capture strictly in preparation order.
- Recover staged and abandoned preparations before Plant accepts proxy traffic
  or permits Sealing, failing startup rather than guessing on conflicting state.
- Seal Capture and Herdr generations through validated, immutable detached
  evidence with base-length and digest proof, including directory-anchored
  no-follow mutation, cooperative cross-process session locking, narrowly
  enumerated temp recovery, and exact-once process- or power-loss retry.
- Require complete ownership of both Plant listeners before recovery or manual
  compression, and run scheduled compression in-process in the owning daemon.
- Propagate shared maintenance traversal failures through every caller instead
  of treating an incomplete inventory as successful work.
- Keep Session Index and Herdr snapshot timing at durable stage acceptance.
- Make Reconstruction recover complete concatenated legacy Envelopes while
  rejecting unrecoverable terminated records and malformed sealed tails, and
  retain one no-follow, length-bounded generation snapshot across concurrent
  Sealing rename and cleanup.
- Supervise preconfigured job and compressor commands through one complete
  deadline covering the direct-child wait and only the requested output pipes,
  with explicit kill and reap before transaction cleanup.
- Preserve the current live-raw exception for one unterminated final fragment.

## Impact

- Affected specs: `capture-stewardship`
- Affected code: `crates/plant/src/capture.rs`,
  `crates/plant/src/capture/persistence.rs`,
  `crates/plant/src/capture/session_fs.rs`,
  `crates/plant/src/capture/generation.rs`, `crates/plant/src/proxy.rs`,
  `crates/plant/src/main.rs`, `crates/plant/src/jobs.rs`,
  `crates/plant/src/process.rs`, `crates/plant/src/sweep.rs`,
  `crates/plant/src/herdr.rs`, `crates/vaultr/src/vault.rs`,
  `crates/vaultr/src/recon.rs`, and focused tests
- Issue traceability:
  - #19: immutable Capture and Herdr generation Sealing with exact-once retry
  - #20: complete two-listener ownership and daemon-only scheduled compression
  - #21: path-exact, strict, fail-closed recovery
  - #22: byte-exact idempotent recovery append reconciliation
  - #30: bounded subprocess timeout with explicit child kill and reap
  - #32: explicit maintenance traversal failure propagation
- Related historical issue: #16 documents the legacy concatenated-record and
  mixed-generation Reconstruction evidence
- Existing proposals: distinct from draft PR #12, whose Plant generation
  lifecycle was superseded by merged PR #14
