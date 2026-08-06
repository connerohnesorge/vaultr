## Implementation Details

Convert `crates/vaultr/src/scan.rs` into a `scan` module directory.

Use these focused modules:

- `scan/mod.rs` owns `ScanArgs`, shared scan types, output formatting, and command orchestration.
- `scan/input.rs` owns Git commands, revision parsing, path validation, and committed blob reads.
- `scan/engine.rs` owns byte scanning, line extraction, finding construction, and report construction.
- `scan/review.rs` owns decisions, review state, and review action handling.
- `scan/server.rs` owns the loopback listener, HTTP parsing, response writing, and browser launch.
- `scan/review.html` stores the embedded review page loaded with `include_str!`.

Keep internal interfaces narrow. Use `pub(super)` only where the parent module or sibling modules need access.

Keep `secrets` as the owner of pattern matching, policy loading, and allowlist mutation.

## Context

The current scan module already delegates pattern matching to `secrets`.

The refactor separates command orchestration from repository, review, and transport concerns.

## Goals / Non-Goals

- Goals: Reduce scan module coupling.
- Goals: Preserve command output and exit codes.
- Goals: Preserve the local review protocol.
- Goals: Improve unit-test boundaries.
- Non-Goals: Change secret patterns.
- Non-Goals: Change allowlist semantics.
- Non-Goals: Add an HTTP dependency.
- Non-Goals: Change the command interface.

## Decisions

- Decision: Keep the loopback server on the standard library.
- Decision: Keep the review page as a compile-time asset.
- Decision: Keep `run` as the public command entry point.

## Risks / Trade-offs

- Moving private types can expose incorrect visibility boundaries. Keep cross-module visibility limited to `pub(super)`.
- Asset extraction can change HTML bytes. Use `include_str!` and verify the review flow after the move.
