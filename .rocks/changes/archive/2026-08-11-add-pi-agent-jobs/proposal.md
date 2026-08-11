# Change: Add Pi agent job launches

## Why

Scheduled Plant jobs cannot launch Pi as a first-class agent. They need Pi without losing native Herdr lifecycle checks or self-capture exclusion.

## What Changes

- Add `pi` as a Plant Agent CLI launch identity.
- Render Pi launches with project trust approval, the Codex provider, and Pi-native model and thinking flags.
- Give each Pi run a unique Plant-managed session directory.
- Register the Pi session record ID as a job self-capture before cleanup.
- Preserve Claude, Codex, and Prime launch behavior.

## Impact

- Affected specs: `plant-agent-jobs`
- Affected code: Plant agent CLI parsing, launch rendering, Herdr lifecycle, and focused tests
