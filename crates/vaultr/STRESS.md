# Reconstruction memory stress test

Proves that `vaultr`'s streaming reconstructor keeps peak RSS bounded by the
largest single envelope plus the final history — not by archive size.

## Setup

- Generator: a Python script producing a synthetic `turns.jsonl` of v1
  claude-code envelopes with repeated append deltas (~40 KiB per envelope,
  two 20 KiB messages each). History grows to a 200-message window and then a
  `prefix_length: 0` compaction collapses it, so the *final* history stays
  bounded while the archive grows without bound — the realistic long-session
  shape.
- Files were written to a temp scratch directory (never the vault) and deleted
  after the run.
- Measured with `/usr/bin/time -l` on the release binary running
  `vaultr --vault <tmp> session show <id> --stats`.

## Results (macOS, 2026-07-15, release build)

| archive size | envelopes | final history | wall time | peak RSS |
|---|---|---|---|---|
| 650 MiB (681,600,630 B) | 16,800 | 200 msgs | 0.26 s | 7,307,264 B (~7.0 MiB) |
| 325 MiB (half, truncated tail) | 8,399 | 198 msgs | 1.63 s | 7,356,416 B (~7.0 MiB) |

Doubling the archive size leaves peak RSS flat at ~7 MiB (the two runs differ
by 0.7%, within noise). The half-size file ends in a truncated envelope,
which also exercises the live-tail ignore path.
