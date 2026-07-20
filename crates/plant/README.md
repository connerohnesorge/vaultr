# plant

Rust rewrite of wireproxy (Bun). Reverse proxy that captures Claude Code /
Codex API traffic into `~/.dotfiles/vault/sessions/`. See `src/main.rs` header
and `~/.dotfiles/.claude/SESSIONS.md` for the behavioral contract.

- Build: `cargo build --release` (binary at `target/release/plant`)
- Health: `curl http://127.0.0.1:18923/health`
- Verify: `target/release/plant --self-test`

## Capture maintenance ownership

Sweep selects validated generation inventories and policy; the private capture
maintenance module alone rechecks journal/stage readiness, detaches Capture and
Herdr generations, and seals them through one retained no-follow
session-directory boundary.

## Surviving restarts (launchd supervision)

Clients (Claude Code's SDK) already retry failed API requests with backoff, so
a torn stream or brief connection-refused window self-heals client-side. Two
server-side pieces make restarts actually safe:

1. **launchd `KeepAlive`** — `~/Library/LaunchAgents/com.cohnesor.plant.plist`
   (stow-linked from `Library/LaunchAgents/` in this repo) respawns plant
   whenever it dies, mid-session included. Previously only the SessionStart
   hook started it, so a crash left live sessions dead until the next session
   launch. plant binds both harness ports before recovery or scheduler work.
   A losing copy exits 0 only when both health endpoints identify a complete
   incumbent plant; partial or foreign ownership fails without mutating
   recovery state. Logs: `~/.local/state/plant/launchd.log`.

2. **Listeners retained through a fixed-set drain** — on SIGTERM/SIGINT, each
   listener stops accepting, closes its owned capture-task tracker, and
   boundedly drains only the connections and capture tee/finalizers accepted
   before cancellation. Client disconnect closes a capture tee even when its
   upstream is indefinitely stalled. At the deadline, remaining connection
   and capture tasks are aborted and reaped. Both listener descriptors stay
   owned until both supervisors join, so a replacement cannot overlap a dying
   instance's capture writes.

Skipped: SO_REUSEPORT / fd handover for true zero-downtime — SDK retries
cover the bounded handover gap; add only if retries observably fail during
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
