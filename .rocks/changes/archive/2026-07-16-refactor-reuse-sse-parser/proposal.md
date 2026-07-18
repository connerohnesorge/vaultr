# Change: Reuse the canonical SSE parser

## Why

Plant telemetry and Vaultr reconstruction contain byte-identical permissive SSE parsers. Plant already depends on Vaultr, so deleting the copy removes drift without adding a module, dependency, or seam.

## What Changes

- Delete `plant::adapter::parse_sse`.
- Reuse the existing public `vaultr::recon::parse_sse` from Plant telemetry.
- Lock the existing permissive parsing behavior with one focused test.
- Do not introduce a generic SSE module, streaming abstraction, trait, or stricter parser.

## Impact

- Affected specs: `capture-telemetry`
- Affected code: `crates/plant/src/adapter.rs`, `crates/plant/src/otel.rs`, focused Vaultr reconstruction tests
- Unchanged: captured response bytes, telemetry fields, reconstruction output, dependencies
