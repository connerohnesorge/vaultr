# plant

Rust rewrite of wireproxy (Bun). Reverse proxy that captures Claude Code /
Codex API traffic into `~/.dotfiles/vault/sessions/`. See `src/main.rs` header
and `~/.dotfiles/.claude/SESSIONS.md` for the behavioral contract.

- Build: `cargo build --release` (binary at `target/release/plant`)
- Health: `curl http://127.0.0.1:18923/health`
- Verify: `target/release/plant --self-test`

## Surviving restarts (launchd supervision)

Clients (Claude Code's SDK) already retry failed API requests with backoff, so
a torn stream or brief connection-refused window self-heals client-side. Two
server-side pieces make restarts actually safe:

1. **launchd `KeepAlive`** — `~/Library/LaunchAgents/com.cohnesor.plant.plist`
   (stow-linked from `Library/LaunchAgents/` in this repo) respawns plant
   whenever it dies, mid-session included. Previously only the SessionStart
   hook started it, so a crash left live sessions dead until the next session
   launch. plant's existing "port already bound → exit 0" behavior makes the
   launchd copy and hook-started copies coexist safely: whoever binds first
   wins, the loser exits 0. Logs: `~/.local/state/plant/launchd.log`.

2. **Listeners released before drain** — on SIGTERM/SIGINT, `main.rs` aborts
   the accept loops (dropping the listeners) *before* the 30s in-flight drain
   sleep. A replacement can bind immediately instead of hitting AddrInUse and
   yielding to a dying instance. Per-connection tasks are independent spawns,
   so in-flight streams still finish.

Skipped: SO_REUSEPORT / fd handover for true zero-downtime — SDK retries
already cover the sub-second gap; add only if retries observably fail during
restarts.

### macOS setup (one-time, already done 2026-07-15)

```sh
# plist lives in the repo, stow links it into the real ~/Library/LaunchAgents
cd ~/.dotfiles && stow -t ~ .
mkdir -p ~/.local/state/plant
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.cohnesor.plant.plist
# if an older non-launchd plant owns the port, hand over:
kill -TERM <old pid>   # launchd copy binds within seconds
curl http://127.0.0.1:18923/health
```

Manage with:

```sh
launchctl print gui/$UID/com.cohnesor.plant   # status
launchctl kickstart -k gui/$UID/com.cohnesor.plant   # restart
launchctl bootout gui/$UID/com.cohnesor.plant  # stop supervision
```

Note: launchd loaded the plist fine as a symlink on macOS 25.x; if a future
OS update rejects symlinked plists, copy the file instead of stowing it.
