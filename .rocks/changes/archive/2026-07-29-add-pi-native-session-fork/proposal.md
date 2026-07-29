# Change: Add Pi native session forks

## Why

Vaultr writes native Claude Code and Codex sessions but cannot write native Pi sessions.
The Pi `/btw` launcher needs an exact fork of its current Session Capture.

## What Changes

- Add Pi as a native fork target.
- Add optional initial-prompt and read-only launch policies.
- Accept Pi's Codex Responses route through Plant.
- Add Pi to the Vaultr Herdr fork action.
- Document the Pi native format and launch behavior.

## Impact

- Affected capabilities: `pi-session-fork`, `capture-stewardship`.
- Affected code: Vaultr fork orchestration, Plant proxy routing, and the Herdr plugin.
- Session Captures remain read-only to Vaultr.
