# vaultr

> Every AI-harness session ends up in its own private silo. vaultr captures the ones
> you run locally and lets you fork any of them into a fresh native session — in Claude
> Code, Codex, or Pi.

## Features

- **Zero-conf capture** — Plant is a local TLS reverse proxy (:18923 Claude,
  :18924 Codex) with its own per-user CA; every API call is recorded into the vault.
- **Native session forks** — fork any captured session into a fresh Claude Code,
  Codex, or Pi session that the target CLI resumes natively.
- **Vault health** — `vaultr validate` checks wikilinks, frontmatter, Markdown
  paths, and the ledger; `vaultr scan` checks committed blobs for secrets.
- **NixOS module** — `nixosModules.default` runs plant as a systemd service and
  routes your `claude` / `codex` into it.

## How it works

```
Claude Code ──HTTPS──► plant :18923 ──► vault/sessions/
Codex       ──HTTPS──► plant :18924 ──► vault/sessions/
vaultr session list | show | fork   (reads the capture, writes native sessions)
```

Two binaries do the work.

- **plant** is a local TLS reverse proxy that captures Claude Code and Codex API
traffic into the vault, on loopback :18923 (Claude) / :18924 (Codex) with its own
per-user CA, so the CLIs connect without system-wide trust changes.
- **vaultr** is the CLI over that captured wire data: list, render, validate, scan,
and fork sessions into native files for any supported harness.

Operational detail — ports, restarts, maintenance ownership, scheduled jobs —
lives in `crates/plant/README.md`.

## NixOS

The flake exports `nixosModules.default`. Enable it for one user and supply the
CLI packages that its routed `claude` and `codex` wrappers execute:

```nix
{
  imports = [inputs.vaultr.nixosModules.default];
  services.vaultr = {
    enable = true;
    user = "dev";
    claudePackage = inputs.llm-agents.packages.${pkgs.system}.claude-code;
    codexPackage = unstablePkgs.codex;
  };
}
```

The module runs Plant as a system service on `127.0.0.1:18923` and
`127.0.0.1:18924`.

## Global flags

| Flag | Purpose |
| --- | --- |
| `--vault <path>` | Sessions root override (default: `$VAULT_SESSIONS` or `~/.dotfiles/vault/sessions`) |
| `--no-fetch` | Fail on a local miss instead of fetching the seal from the S3 store |

### Sealed captures

Old sessions can be *sealed*: the transcript bytes move to an S3 store of
record while `sessions/.meta/<id>.json` stays in git, so every session stays
listed and discoverable by clone. The read verbs (`session show`, `path`,
`fork`, `herdr`) fetch sealed bytes on demand; nothing on the capture or sweep
path ever fetches. Pass `--no-fetch` to work strictly offline.

## CLI reference

| Command | Purpose |
| --- | --- |
| `vaultr session list` | List captured sessions (current cwd; `--all` for every cwd) |
| `vaultr session show <id>` | Render the transcript as Markdown |
| `vaultr session path <id>` | Print the session directory (`--copy` copies it) |
| `vaultr session herdr <id>` | Stream the session's topology snapshots as JSONL |
| `vaultr session fork <id> --into …` | Fork into a fresh native Claude/Codex/Pi session (`--cwd`, `--prompt`, `--read-only`, `--no-launch`) |
| `vaultr validate` | Validate the vault (`--json`, `--strict`) |
| `vaultr scan` | Scan committed text blobs for secrets |

## Native session forks

Fork a captured session into Claude Code, Codex, or Pi — vaultr reads the
Session Capture and writes a fresh native target session in the captured working
directory:

```bash
vaultr session fork <session-id> --into pi
vaultr session fork <session-id> --into claude --read-only --prompt "review this"
vaultr session fork <session-id> --into codex --no-launch
```

Vaultr reads only the Session Capture — it never rewrites it. The fork writes a
fresh native target session and launches it in the captured working directory unless
`--cwd` overrides it.

Forks are native: they emit real on-disk session files (Claude Code jsonl, Codex
v7-uuid database, Pi typed entry chains) documented in `docs/native-formats.md`, so
they appear in the target harness's normal resume picker.

## Plant service

Plant runs on loopback `127.0.0.1:18923` (Claude) and `127.0.0.1:18924`
(Codex).

- Health: `curl http://127.0.0.1:18923/health`
- Self-test: `plant --self-test`
- Captures any session opened through it into `<vault>/sessions/`
- Scheduled jobs scan `<vault>/jobs/` every minute; a `.hostname` file limits
  flat jobs to that host, `jobs/shared/` runs on every host.

Plant also ships operational subcommands beyond the daemon:

- `plant agent-run` — run a one-shot agent (CLI, model, effort, timeout,
  idempotency key) under capture
- `plant jobs run|unblock|worker` — drive the scheduled job set manually or as
  a worker
- `plant credentials reconcile` — reconcile broker/seal-store state
- `plant sessions eligible|stuck` — eligibility and stuck-capture scans

### vaultr-door

`ts/vaultr-door` is a Plant job (TypeScript) that watches the ingestion root
and turns new files into durable agent runs, with state locking, claim keys,
and receipts.

## Herdr plugin

`herdr-plugin.toml` registers a Herdr plugin (`plugin/`) with three pane
actions over captured sessions: **copy-path**, **show** (transcript), and
**fork** — which opens a focused split, asks claude/codex/pi, and forks the
pane's session into it in place.

Maintenance and restart-supervision detail: `crates/plant/README.md`.

## Security & architecture

- **plant** runs on every box where agents run (that is, inside every agent
  sandbox) and holds no credentials.
- **plant-broker** is the only holder of the credentials reaching the seal store
  and is a separate binary that never imports `plant` (nor vice-versa) — so the
  seal-store grant never lands on a machine that runs untrusted agent code.

The broker is deliberately a cluster artifact: it is the one binary that may
hold the seal-store credentials, and it is excluded from the workspace flake and
Docker builds — the laptop artifact ships `vaultr` + `plant` only.

## Development

```bash
nix develop            # dev shell
cargo build --release  # builds vaultr + plant
cargo test --workspace
```

Deep reference: `docs/native-formats.md` documents the Claude Code / Codex / Pi
on-disk session formats that the fork writers emit.

## Credits & license

- Session formats: Claude Code, Codex CLI, and Pi — see
  `docs/native-formats.md`.
- Secrets scanning draws on ripsecrets (third-party notice in `LICENSE`).
- MIT licensed.

