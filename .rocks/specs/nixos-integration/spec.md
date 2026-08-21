# Nixos Integration Specification

## Requirements

### Requirement: NixOS Plant service

Vaultr MUST export `nixosModules.default`.
The module MUST run Plant as one configured user.
The module MUST restart Plant after failures.
The module MUST allow Plant thirty seconds to drain during shutdown.

#### Scenario: Plant runs for the configured user

- WHEN `services.vaultr.enable` is true
- AND `services.vaultr.user` names an existing user
- THEN systemd runs Plant with that user's home directory
- AND systemd restarts Plant after an unexpected exit

#### Scenario: Plant shuts down

- WHEN systemd stops Plant
- THEN systemd allows at least thirty seconds for Plant's bounded drain

### Requirement: Captured agent command wrappers

The module MUST install `claude` and `codex` wrappers.
The Claude Code wrapper MUST route requests to Plant's Claude Code listener.
The Codex wrapper MUST route requests to Plant's Codex listener.
Each wrapper MUST execute the configured upstream package.

#### Scenario: Claude Code starts

- WHEN the configured user runs `claude`
- THEN the wrapper sets `ANTHROPIC_BASE_URL` to `http://127.0.0.1:18923`
- AND the wrapper executes the configured Claude Code package

#### Scenario: Codex starts

- WHEN the configured user runs `codex`
- THEN the wrapper passes `openai_base_url="http://127.0.0.1:18924"`
- AND the wrapper executes the configured Codex package

