## Context

Plant binds one Claude Code port and one Codex port per machine.
The NixOS service therefore runs once for one configured user.

## Decisions

- Export one `services.vaultr` module with `enable`, `user`, `package`, `claudePackage`, and `codexPackage` options.
- Install wrappers named `claude` and `codex`.
- Set `ANTHROPIC_BASE_URL` in the Claude Code wrapper.
- Pass the `openai_base_url` override in the Codex wrapper.
- Run Plant as a system service with the selected user's home directory.
- Load flat jobs when `jobs/.hostname` matches the short hostname.
- Preserve flat job behavior when `jobs/.hostname` is absent.
- Load jobs from `jobs/shared/` on every hostname.

## Risks

- Plant starts before a user's vault exists.
  The system service restarts until the configured capture root becomes available.
- A duplicate name across flat and shared jobs shares one ledger.
  The shipped vault uses unique names.
