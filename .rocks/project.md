# vaultr Context

## Purpose

Vaultr preserves Claude Code and Codex sessions as durable captures, reconstructs and forks them into native harness formats, validates knowledge-vault content, and ships Plant as the resident capture and cultivation runtime.

## Tech Stack

- Rust 2021 Cargo workspace with `vaultr` and `plant` crates
- Tokio, Hyper, and Reqwest for Plant's async reverse proxy and job runtime
- Serde JSON and Zstandard for capture storage and reconstruction
- Clap for the Vaultr CLI
- Nix flakes for packaging both binaries
- POSIX shell for the Herdr plugin actions

## Project Conventions

### Code Style

- Prefer concrete modules and standard-library types; add a seam only when behavior actually varies.
- Keep Vaultr deterministic and read-only against session captures and vault content.
- Keep Plant failure paths non-fatal where capture uptime is at stake.
- Preserve unknown additive harness fields when reconstructing native history.

### Architecture Patterns

- Vaultr is the deterministic core: session discovery, reconstruction, rendering, native writing, and read-only content validation.
- Plant is the opinionated automation layer: capture writing, security scrubbing, sealing, scheduling, and cultivation-agent orchestration.
- Dependency direction is `plant -> vaultr`; both binaries remain in this workspace until ownership or deployment actually diverges.
- Same-harness forks preserve native wire history; cross-harness forks pass through the normalized transcript and best-effort tool translation.
- Plant owns security scrubbing because it is the sole capture writer and sealing caller.

### Testing Strategy

- Run `cargo test --workspace` for the complete automated suite.
- Keep parser and policy checks as focused unit tests; use integration tests for reconstruction, rendering, and native writers.
- Exercise effectful Herdr lifecycle changes against a real live Herdr session instead of adding a fake command adapter solely for tests.
- Keep `plant --self-test` as the end-to-end proxy, capture, telemetry, and scrub check.

### Git Workflow

- Work from `main` with small conventional commits.
- Architecture changes start as validated Rocks proposals and are not implemented until accepted.
- Preserve unrelated working-tree changes.

## Domain Context

Canonical terms and relationships live in `.rocks/CONTEXT.md`.

## Important Constraints

- Session capture is appendable while live, may be security-scrubbed before sealing, and is stable after sealing.
- `.meta/<session-id>.json` is authoritative for Vaultr identity and discovery; dated capture directories own content after resolution.
- Native writers use fresh IDs, atomic mode-0600 creation, and never overwrite existing sessions.
- Native writers emit the conservative verified format subset without an installed-version gate; real resume smoke tests detect drift.
- Plant jobs must never type a prompt into an unverified shell, steal focus, or close a working agent workspace.

## External Dependencies

- Claude Code and Codex native session formats
- Herdr CLI and Unix socket
- Zstandard CLI for Plant sealing
- macOS launchd for the resident Plant process
- Optional OTLP endpoint for Plant telemetry
