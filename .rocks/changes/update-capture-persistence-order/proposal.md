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
- Keep Session Index and Herdr snapshot timing at durable stage acceptance.
- Make Reconstruction recover complete concatenated legacy Envelopes while
  rejecting unrecoverable terminated records and malformed sealed tails.
- Preserve the current live-raw exception for one unterminated final fragment.

## Impact

- Affected specs: `capture-stewardship`
- Affected code: `crates/plant/src/capture.rs`,
  `crates/plant/src/capture/persistence.rs`, `crates/plant/src/proxy.rs`,
  `crates/plant/src/main.rs`, `crates/plant/src/jobs.rs`,
  `crates/plant/src/sweep.rs`, `crates/vaultr/src/vault.rs`,
  `crates/vaultr/src/recon.rs`, and focused tests
- Related issue: https://github.com/connerohnesorge/vaultr/issues/16
- Existing proposals: distinct from draft PR #12, whose Plant generation
  lifecycle was superseded by merged PR #14
