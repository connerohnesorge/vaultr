# Change: Split the secret scan module

## Why

The scan command combines repository input, scan results, review state, HTTP transport, and UI assets in one 590-line module.

Focused modules will make each responsibility easier to test and maintain.

## What Changes

- Move Git range and committed blob handling into an input module.
- Move finding and report construction into a scan engine module.
- Move review state and review actions into a review module.
- Move local HTTP transport into a server module.
- Move the embedded review page into a standalone asset.
- Preserve the current command interface and behavior.

## Impact

- Affected specs: `vaultr-scan`
- Affected code: `crates/vaultr/src/scan.rs`, `crates/vaultr/src/main.rs`, and new scan modules
- Dependencies: None
