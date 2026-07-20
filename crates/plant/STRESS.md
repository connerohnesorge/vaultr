# Coverage memory stress test

Proves that `plant sessions coverage` keeps peak RSS bounded by the largest
Envelope record plus the two request-ID sets, rather than decoded capture size.
This complements `coverage::tests::handles_large_records`, which checks only
large-record correctness.

## Method

Build the release CLI, then generate two zstd-only Session Captures. Both
fixtures have exactly two capture IDs, the same two-line native transcript, and
the same 1 MiB padding field per Envelope. Only the number of Envelopes changes:
128 versus 919. Python writes each Envelope to stdout and `zstd` consumes that
stream, so no decoded archive is written to disk.

```sh
cargo build --release -p plant

ROOT=$(mktemp -d /tmp/plant-coverage-rss.XXXXXX)
BIN="$PWD/target/release/plant"

make_fixture() {
  sid="$1"
  records="$2"
  dir="$ROOT/2026/07/20/$sid"
  mkdir -p "$dir" "$ROOT/.meta"
  python3 - "$ROOT" "$sid" "$records" <<'PY' |
    zstd -q -1 -T1 -o "$dir/turns.jsonl.zst"
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
sid = sys.argv[2]
records = int(sys.argv[3])
transcript = root / f"{sid}-transcript.jsonl"
with transcript.open("w") as out:
    for request_id in ("req_A", "req_B"):
        out.write(json.dumps({
            "type": "assistant",
            "requestId": request_id,
            "timestamp": "2026-07-20T12:00:00.000Z",
        }, separators=(",", ":")) + "\n")
meta = {
    "session_id": sid,
    "original_start": "2026-07-20T12:00:00.000Z",
    "session_start_source": "wire",
    "transcript_path": str(transcript),
}
(root / ".meta" / f"{sid}.json").write_text(
    json.dumps(meta, separators=(",", ":")) + "\n"
)
padding = "x" * (1024 * 1024)
for index in range(records):
    envelope = {
        "harness": "claude-code",
        "observed_at": "2026-07-20T12:00:00.000Z",
        "padding": padding,
        "response": {
            "headers": {
                "request-id": f"req_{'A' if index % 2 == 0 else 'B'}"
            }
        },
    }
    sys.stdout.write(json.dumps(envelope, separators=(",", ":")) + "\n")
PY
}

SMALL=019f1234-5678-7abc-8def-0123456789c1
LARGE=019f1234-5678-7abc-8def-0123456789c2
make_fixture "$SMALL" 128
make_fixture "$LARGE" 919

for sid in "$SMALL" "$LARGE"; do
  capture="$ROOT/2026/07/20/$sid/turns.jsonl.zst"
  zstd -q -dc "$capture" | wc -c
  stat -f %z "$capture"
  for run in 1 2 3; do
    /usr/bin/time -l env VAULT_SESSIONS="$ROOT" \
      "$BIN" sessions coverage "$sid" >/dev/null
  done
done
```

The line break before `zstd` in `make_fixture` is intentional: the Python
heredoc is piped directly to the compressor. Remove the generated scratch tree
after recording the results.

## Results

Measured 2026-07-20 on macOS 26.5.2 (25F84), arm64, Rust 1.94.1, zstd 1.5.7.
`/usr/bin/time -l` reported maximum resident set size in bytes.

| Envelopes | Decoded capture | zstd file | Request IDs | Peak RSS runs | Median RSS |
|---:|---:|---:|---:|---:|---:|
| 128 | 134,233,856 B (128.015 MiB) | 19,267 B | 2 | 12,140,544; 13,172,736; 13,172,736 B | 13,172,736 B |
| 919 | 963,757,138 B (919.110 MiB) | 137,093 B | 2 | 13,320,192; 12,255,232; 13,303,808 B | 13,303,808 B |

Both commands printed `coverage 100.0% (2/2 in-window)`. Scaling decoded
capture size by 7.18x increased median RSS by 131,072 bytes (1.00%); the
919.110 MiB run peaked at 13,320,192 bytes (12.70 MiB). Generated fixtures were
deleted and are not committed.
