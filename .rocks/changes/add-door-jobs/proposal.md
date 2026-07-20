# Add Door Jobs

## Why

Teams messages and email already land in the vault as files via half-hour sync
jobs, but nothing turns an interesting new file into work. Doors close that
gap: a door watches ingested Vault Content and launches a Herdr agent session
over the new files, so a message can start (and, through the existing
`plant agent run` path, chain) agent work without a human at the keyboard.

## What Changes

- Plant's job scanner becomes polyglot: jobs are executable
  `<name>.<interval>.<ext>` files exec'd directly via their shebang instead of
  being forced through `/bin/bash`, so a door can be a TypeScript job run by
  Bun with no interpreter special-casing in Plant.
- A new Bun/TypeScript library in this workspace (`ts/`) owns the door
  routine: new-file detection against an ordered per-door cursor, a durable
  in-progress batch claim under a cross-process lock, an ingestion-only
  allowlist of watchable roots, a rolling-window fire breaker, and a typed
  idempotent wrapper over `plant agent run` mirroring the
  `Unavailable`/`Failed`/`Succeeded` outcome contract.
- A door is then an ordinary job script (`door-<name>.<interval>.ts`) of ~10
  lines: watch glob, filter predicate, prompt builder. Batch firing — one
  agent session per door per run, all new matches in one prompt.
- A Teamer sync job is added to the vault jobs directory so Teams chats become
  watchable Vault Content the way Mailer already makes email watchable.

## Impact

- Affected specs: `plant-agent-jobs` (polyglot execution added), new
  capability `vault-doors`.
- Affected code: `crates/plant/src/jobs.rs` (filename parse + exec), new
  `ts/` Bun package, flake packaging, `vault/jobs/AGENTS.md` contract
  (executable + shebang required), one-time shebang sweep of existing `.sh`
  jobs.
- Not changed: the Herdr lifecycle stays implemented once, in Plant; doors
  never drive panes except through `plant agent run`.
