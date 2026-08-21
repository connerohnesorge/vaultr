# Change: Add a NixOS capture module

## Why

Vaultr only provides package outputs and a macOS launchd deployment.
NixOS VM images need one reusable module for Plant capture and CLI routing.

## What Changes

- Export `nixosModules.default` from the Vaultr flake.
- Run Plant as one selected NixOS user.
- Route Claude Code and Codex through Plant wrappers.
- Add host-scoped root jobs and portable shared jobs.

## Impact

- Affected specs: `nixos-integration`, `plant-agent-jobs`
- Affected code: Nix flake outputs, Plant job discovery, focused tests
