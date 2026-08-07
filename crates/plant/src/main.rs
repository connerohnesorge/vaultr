//! plant — Rust rewrite of wireproxy (Bun). Same behavioral contract:
//! see ~/.dotfiles/.claude/plans/handoff-redesign-wireproxy-linked-mountain.md
//! and the original ~/.dotfiles/.config/wireproxy/wireproxy.ts.

mod adapter;
mod agent_run;
mod ca;
mod capture;
mod cli;
mod coverage;
mod credentials;
mod domain;
mod fsutil;
mod herdr;
mod jobs;
mod otel;
mod process;
mod proxy;
mod selftest;
mod state;
mod sweep;

use cli::Command;
use domain::Harness;
use proxy::ProxyCtx;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn crash_log() -> PathBuf {
    let dir = state::dir();
    let _ = state::ensure_dir_durable(&dir);
    dir.join("crash.log")
}

/// Sessions root via the shared vaultr resolver ($VAULT_SESSIONS > ~/.dotfiles/vault/sessions).
fn vault_root() -> PathBuf {
    vaultr::vault::root(None).unwrap_or_else(|error| {
        eprintln!("[plant] cannot resolve vault sessions root: {error}");
        std::process::exit(1);
    })
}

fn usage(error: &str) -> i32 {
    eprintln!("plant: {error}");
    eprintln!(
        "usage: plant [--self-test | server stop | sessions eligible|coverage|stuck ... | \
         compress once ... | jobs run|unblock <name>|worker ... | agent run --cli claude|codex ... | \
         credentials reconcile [--once | --interval <dur>] [--source <dir>]]"
    );
    2
}

fn credentials_usage() {
    println!(
        "usage: plant credentials reconcile [--once | --interval <dur>] [--source <dir>]\n\
         \n\
         Reconciles projected Kubernetes credentials into guest credential files,\n\
         rewriting the configs derived from them (git-credentials, glab config.yml).\n\
         \n\
           --once            apply a single pass and exit; non-zero if any entry failed\n\
           --interval <dur>  poll the source for changes (default 30s)\n\
           --source <dir>    projected credential directory (default {}, \n\
                            overridable with COMPUTERS_CREDENTIAL_DIR)",
        credentials::DEFAULT_SOURCE
    );
}

async fn dispatch(command: Command) -> i32 {
    match command {
        Command::Daemon => {
            run_daemon().await;
            0
        }
        Command::ServerStop => server_stop().await,
        Command::SelfTest => {
            selftest::self_test().await;
            0
        }
        Command::SessionsEligible(args) => {
            let vault = vault_root();
            let list = match args.claim {
                Some(lease) => match sweep::eligible_and_claim(
                    &vault,
                    args.idle,
                    args.max,
                    args.learner,
                    lease,
                ) {
                    Ok(list) => list,
                    Err(error) => {
                        eprintln!("sessions eligible: claim failed: {error}");
                        return 2;
                    }
                },
                None => match sweep::eligible_sessions(&vault, args.idle, args.max, args.learner) {
                    Ok(list) => list,
                    Err(error) => {
                        eprintln!("sessions eligible: inventory failed: {error}");
                        return 2;
                    }
                },
            };
            let (total, ledgered) = match sweep::eligibility_stats(&vault, args.learner) {
                Ok(stats) => stats,
                Err(error) => {
                    eprintln!("sessions eligible: inventory failed: {error}");
                    return 2;
                }
            };
            eprintln!(
                "[eligible:{}] {} of {total} sessions ({ledgered} ledgered)",
                args.learner.ledger_label(),
                list.len()
            );
            if list.is_empty() {
                return 1;
            }
            println!(
                "{}",
                list.iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            0
        }
        Command::SessionsCoverage(sid) => match coverage::coverage(&vault_root(), &sid) {
            Ok(coverage) => {
                let tag = if coverage.resumed { " (resumed)" } else { "" };
                println!(
                    "{} coverage {:.1}% ({}/{} in-window){tag}",
                    coverage.sid,
                    coverage.pct(),
                    coverage.in_window_native - coverage.missing.len(),
                    coverage.in_window_native,
                );
                println!(
                    "  window_start={} carryover={}",
                    coverage.window_start, coverage.carryover
                );
                if coverage.recorded_drops > 0 {
                    println!(
                        "  KNOWN-INCOMPLETE: {} recorded dropped turn(s)",
                        coverage.recorded_drops
                    );
                }
                if coverage.captured > coverage.in_window_native {
                    println!("  captured={} (>= in-window native)", coverage.captured);
                }
                if coverage.missing.is_empty() {
                    println!("  no in-window gap");
                    return 0;
                }
                println!(
                    "  missing {} in-window request-id(s):",
                    coverage.missing.len()
                );
                for request_id in &coverage.missing {
                    println!("    {request_id}");
                }
                1
            }
            Err(error) => {
                eprintln!("sessions coverage: {error}");
                1
            }
        },
        Command::SessionsStuck(age) => {
            let vault = vault_root();
            if let Some(alert) = sweep::headroom_alert(&vault) {
                println!("{alert}");
            }
            for alert in sweep::dropped_turn_alerts(&vault) {
                println!("{alert}");
            }
            let stuck = match sweep::stuck_captures(&vault, age) {
                Ok(stuck) => stuck,
                Err(error) => {
                    eprintln!("sessions stuck: inventory failed: {error}");
                    return 2;
                }
            };
            for capture in &stuck {
                println!(
                    "{} {} idle={}h",
                    capture.state,
                    capture.sid,
                    capture.idle_secs / 3600
                );
            }
            println!("{}", sweep::stuck_summary(&stuck));
            i32::from(stuck.iter().any(|capture| capture.state.is_actionable()))
        }
        Command::CompressOnce(idle) => {
            let ownership = match ListenerOwnership::bind_all().await {
                Ok(ownership) => ownership,
                Err(error) => {
                    eprintln!("compress once: listener ownership unavailable: {error}");
                    return 2;
                }
            };
            let vault = vault_root();
            let _ownership = match ownership.recover(&vault) {
                Ok(ownership) => ownership,
                Err(error) => {
                    eprintln!("compress once: capture recovery failed: {error}");
                    return 2;
                }
            };
            match sweep::compress_sweep(&vault, idle).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("compress once: {error}");
                    if matches!(error, sweep::CompressError::Inventory(_)) {
                        2
                    } else {
                        1
                    }
                }
            }
        }
        Command::JobsRun(name) => {
            match jobs::load_jobs().into_iter().find(|job| job.name == name) {
                Some(job) => jobs::run_job(&job).await,
                None => {
                    eprintln!(
                        "unknown job '{name}' (scripts: {})",
                        jobs::load_jobs()
                            .iter()
                            .map(|job| job.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    1
                }
            }
        }
        Command::JobsUnblock(name) => match jobs::unblock_job(&name) {
            Ok(jobs::Unblocked::NoFence) => {
                println!("[job:{name}] no attempt fence, nothing to unblock");
                0
            }
            Ok(jobs::Unblocked::AlreadyClear) => {
                println!("[job:{name}] fence resolves on its own; the next tick clears it");
                0
            }
            Ok(jobs::Unblocked::Cleared(id)) => {
                println!("[job:{name}] recorded attempt {id} failed and cleared the fence");
                0
            }
            Err(error) => {
                eprintln!("[job:{name}] unblock failed: {error}");
                1
            }
        },
        Command::JobsWorker(args) => jobs::run_scheduled_worker(args).await,
        Command::CredentialsHelp => {
            credentials_usage();
            0
        }
        Command::CredentialsReconcile(args) => {
            if args.once {
                credentials::reconcile_once(&args.source)
            } else {
                // Diverges: a supervised reconciler that returns is a box that
                // silently stopped refreshing.
                credentials::reconcile_loop(&args.source, args.interval)
            }
        }
        Command::AgentRun(args) => {
            // No nesting: a job-spawned agent must never spawn another (jobs spawning jobs).
            // launch_line stamps PLANT_AGENT=1 into every agent plant starts; if it's set here
            // we're already inside one, so refuse rather than fork a duplicate.
            if std::env::var_os("PLANT_AGENT").is_some() {
                eprintln!(
                    "agent run: refused — already inside a plant-spawned agent; \
                     do the work in-process, don't spawn another agent"
                );
                return 1;
            }
            let mut prompt = String::new();
            use std::io::Read;
            if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
                eprintln!("agent run: prompt expected on stdin");
                return 1;
            }

            let mut launch =
                jobs::launch_line(args.cli, args.model.as_deref(), args.args.as_deref());
            let mut preset_session_id = None;
            if args.cli == Harness::ClaudeCode {
                let session_id = uuid::Uuid::new_v4().to_string();
                launch.push_str(&format!(" --session-id '{session_id}'"));
                preset_session_id = Some(session_id);
            }
            let vault = vault_root();
            let run = herdr::AgentRun {
                label: args.label.unwrap_or_else(|| "agent".to_string()),
                cwd: args.cwd.unwrap_or_else(|| {
                    format!("{}/.dotfiles", std::env::var("HOME").unwrap_or_default())
                }),
                launch,
                prompt: prompt.trim().to_string(),
                timeout: args.timeout,
                cleanup: jobs::cleanup_policy(args.cleanup, &jobs::Cfg::load(&vault)),
                preset_session_id,
                discover_session_id: args.cli == Harness::Codex,
                env: std::env::var("VAULT_PROJECT_DIGEST")
                    .map(|v| vec![("VAULT_PROJECT_DIGEST".to_string(), v)])
                    .unwrap_or_default(),
            };
            if let Some(key) = args.idempotency_key {
                let receipt = agent_run::run_idempotent(run, &key).await;
                println!(
                    "{}",
                    serde_json::to_string(&receipt).expect("receipt serializes")
                );
                return receipt.exit_code();
            }

            let label = run.label.clone();
            match agent_run::AgentRunReceipt::untracked(herdr::run_agent(run).await) {
                agent_run::AgentRunReceipt::UntrackedSucceeded { detail } => {
                    println!("[agent:{label}] succeeded: {detail}");
                    0
                }
                agent_run::AgentRunReceipt::Retryable { .. } => {
                    println!("[agent:{label}] herdr unavailable");
                    75
                }
                agent_run::AgentRunReceipt::UntrackedFailed { detail } => {
                    println!("[agent:{label}] failed: {detail}");
                    1
                }
                _ => unreachable!("untracked run produced a durable receipt"),
            }
        }
    }
}

/// launchd job label for the Home Manager–supervised plant daemon.
// ponytail: fixed on this machine; add a PLANT_LAUNCHD_LABEL env override if plant ever ships wider.
const LAUNCHD_LABEL: &str = "com.cohnesor.plant";

/// Stop the running server for real. launchd supervises plant with KeepAlive=true,
/// so a bare SIGTERM just respawns — `bootout` sends SIGTERM (plant drains up to 30s)
/// *and* removes the job from supervision, so it stays down.
async fn server_stop() -> i32 {
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}/{LAUNCHD_LABEL}");
    let result = process::run30(&["launchctl", "bootout", &domain]).await;
    if result.ok {
        println!(
            "[plant] server stopped ({domain}); re-enable: \
             launchctl bootstrap gui/{uid} ~/Library/LaunchAgents/{LAUNCHD_LABEL}.plist"
        );
        return 0;
    }
    // bootout on an unloaded job reports ESRCH (3) — already stopped, not a failure.
    if matches!(result.end, process::RunEnd::Exited(Some(3)))
        || result.stderr.contains("No such process")
    {
        println!("[plant] server already stopped ({domain})");
        return 0;
    }
    eprintln!("[plant] server stop failed: {}", result.failure_detail());
    1
}

fn rss_mb() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn record_exit(started: SystemTime, why: &str) {
    let up_s = started
        .elapsed()
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let line = format!(
        "\n----- exit {} pid={} why={why} rss={}MB uptime={up_s}s -----\n",
        capture::iso_now(),
        std::process::id(),
        rss_mb(),
    );
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crash_log())
    {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("reqwest client")
}

/// Budget for one incumbent /health probe. A saturated host starves a *healthy*
/// incumbent well past a second, and a false negative here is expensive: the
/// challenger declares incomplete ownership and exits 1, so launchd's KeepAlive
/// respawns it into the same losing probe — a crash loop caused by load alone,
/// while the incumbent is serving 200s the whole time. Generous and retried.
const INCUMBENT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const INCUMBENT_PROBE_ATTEMPTS: u32 = 3;
const INCUMBENT_PROBE_BACKOFF: Duration = Duration::from_millis(500);

fn incumbent_probe_timeout() -> Duration {
    std::env::var("VAULTR_INCUMBENT_PROBE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(INCUMBENT_PROBE_TIMEOUT)
}

async fn complete_incumbent() -> bool {
    let client = http_client();
    let timeout = incumbent_probe_timeout();
    for adapter in adapter::adapters() {
        if !incumbent_owns(&client, &adapter, timeout).await {
            return false;
        }
    }
    true
}

/// Probe one adapter's /health. Transport failures are retried — they are the
/// symptom of a starved incumbent, not an absent one. A body that parses but
/// does not match is a definitive answer (someone else holds the port), so it
/// short-circuits rather than burning the remaining attempts.
async fn incumbent_owns(
    client: &reqwest::Client,
    adapter: &adapter::Adapter,
    timeout: Duration,
) -> bool {
    let url = format!("http://127.0.0.1:{}/health", adapter.port);
    for attempt in 0..INCUMBENT_PROBE_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(INCUMBENT_PROBE_BACKOFF).await;
        }
        let response = match tokio::time::timeout(timeout, client.get(url.as_str()).send()).await {
            Ok(Ok(response)) if response.status().is_success() => response,
            _ => continue,
        };
        match response.json::<serde_json::Value>().await {
            Ok(health) => return health_matches(&health, adapter),
            Err(_) => continue,
        }
    }
    eprintln!(
        "[plant] incumbent on 127.0.0.1:{} did not answer /health in {} attempt(s) of {:?}",
        adapter.port, INCUMBENT_PROBE_ATTEMPTS, timeout
    );
    false
}

fn health_matches(health: &serde_json::Value, adapter: &adapter::Adapter) -> bool {
    health.get("service").and_then(|value| value.as_str()) == Some("plant")
        && health.get("ok").and_then(|value| value.as_bool()) == Some(true)
        && health.get("harness").and_then(|value| value.as_str())
            == Some(adapter.harness.capture_label())
        && health.get("upstream").and_then(|value| value.as_str())
            == Some(adapter.upstream.trim_end_matches('/'))
}

type BoundServer = (tokio::net::TcpListener, u16, adapter::Adapter);

struct ListenerOwnership {
    servers: Vec<BoundServer>,
}

struct RecoveredListenerOwnership {
    servers: Vec<BoundServer>,
}

impl ListenerOwnership {
    async fn bind_all() -> std::io::Result<Self> {
        let mut servers = Vec::new();
        for adapter in adapter::adapters() {
            match proxy::bind(adapter.port).await {
                Ok((listener, port)) => servers.push((listener, port, adapter)),
                Err(error) => {
                    drop(servers);
                    return Err(error);
                }
            }
        }
        Ok(Self { servers })
    }

    fn recover(self, vault: &std::path::Path) -> Result<RecoveredListenerOwnership, String> {
        self.recover_with(|| capture::recover_all(vault))
    }

    fn recover_with(
        self,
        recover: impl FnOnce() -> Result<(), String>,
    ) -> Result<RecoveredListenerOwnership, String> {
        recover()?;
        Ok(RecoveredListenerOwnership {
            servers: self.servers,
        })
    }
}

impl RecoveredListenerOwnership {
    fn into_servers(self) -> Vec<BoundServer> {
        self.servers
    }
}

fn drain_sweep_interval() -> Duration {
    std::env::var("VAULTR_CAPTURE_DRAIN_SWEEP_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(180))
}

async fn run_daemon() {
    let started = SystemTime::now();
    std::panic::set_hook(Box::new(move |info| {
        let message = format!(
            "\n===== panic {} pid={} =====\n{info}\n",
            capture::iso_now(),
            std::process::id()
        );
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crash_log())
        {
            let _ = file.write_all(message.as_bytes());
        }
        eprintln!("[plant] {info}");
        record_exit(started, "panic");
        std::process::exit(1);
    }));

    let vault = vault_root();
    let ownership = match ListenerOwnership::bind_all().await {
        Ok(ownership) => ownership,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if complete_incumbent().await {
                println!("[plant] complete incumbent owns both harnesses — exiting 0");
                record_exit(started, "exit:0");
                std::process::exit(0);
            }
            eprintln!("[plant] incomplete listener ownership: {error}");
            record_exit(started, "exit:1");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("[plant] bind failed: {error}");
            record_exit(started, "exit:1");
            std::process::exit(1);
        }
    };
    for (_, port, adapter) in &ownership.servers {
        println!(
            "vaultr [{}] 127.0.0.1:{port} -> {}",
            adapter.harness.capture_label(),
            adapter.upstream
        );
    }

    let ownership = ownership.recover(&vault).unwrap_or_else(|error| {
        eprintln!("[plant] capture recovery failed: {error}");
        record_exit(started, "exit:1");
        std::process::exit(1);
    });

    let otel = Arc::new(otel::Otel::new());
    let client = http_client();
    println!("vault={}", vault.display());

    // Eager, and fatal on failure. Clients read the CA once at their own
    // startup via NODE_EXTRA_CA_CERTS, so it has to exist before the first
    // `claude` runs — and a Plant serving CONNECT with no CA would answer 200
    // and then fail every handshake, which reads as a network fault.
    let ca = match ca::Ca::load_or_create() {
        Ok(ca) => {
            println!("ca={}", ca.cert_path().display());
            match ca.bundle_path() {
                Some(bundle) => println!("ca-bundle={}", bundle.display()),
                // macOS has no such file and does not need one; on Linux this
                // means SSL_CERT_FILE consumers will not trust the local CA.
                None => println!("ca-bundle=none (no system bundle found)"),
            }
            Arc::new(ca)
        }
        Err(error) => {
            eprintln!("[plant] CA init failed: {error}");
            record_exit(started, "exit:1");
            std::process::exit(1);
        }
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut accept_loops = tokio::task::JoinSet::new();
    for (listener, _, adapter) in ownership.into_servers() {
        let ctx = Arc::new(ProxyCtx {
            adapter,
            vault: vault.clone(),
            client: client.clone(),
            otel: otel.clone(),
            ca: ca.clone(),
        });
        accept_loops.spawn(proxy::serve_until_shutdown(
            listener,
            ctx,
            shutdown_rx.clone(),
            Duration::from_secs(30),
        ));
    }

    if otel.enabled {
        println!("otel={} every 60s", otel.endpoint);
        let otel = otel.clone();
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                otel.flush(&client, None).await;
            }
        });
    }

    // Drain backlogs stranded behind a dead reservation without waiting for a
    // restart — a long-lived session never gets one.
    {
        let vault = vault.clone();
        let interval = drain_sweep_interval();
        let min_age = proxy::capture_idle_timeout();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let vault = vault.clone();
                match tokio::task::spawn_blocking(move || capture::recover_live(&vault, min_age))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("[plant] drain sweep failed: {error}"),
                    Err(error) => eprintln!("[plant] drain sweep task failed: {error}"),
                }
            }
        });
    }

    tokio::spawn(jobs::scheduler(jobs::Cfg::load(&vault), vault.clone()));
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            println!("[plant] rss={}MB", rss_mb());
        }
    });

    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("signal");
    let mut int = signal(SignalKind::interrupt()).expect("signal");
    let mut hup = signal(SignalKind::hangup()).expect("signal");
    let mut quit = signal(SignalKind::quit()).expect("signal");
    let mut usr1 = signal(SignalKind::user_defined1()).expect("signal");
    let mut usr2 = signal(SignalKind::user_defined2()).expect("signal");
    let why = tokio::select! {
        _ = term.recv() => "signal:SIGTERM",
        _ = int.recv() => "signal:SIGINT",
        _ = hup.recv() => "signal:SIGHUP",
        _ = quit.recv() => "signal:SIGQUIT",
        _ = usr1.recv() => "signal:SIGUSR1",
        _ = usr2.recv() => "signal:SIGUSR2",
    };
    if matches!(why, "signal:SIGTERM" | "signal:SIGINT") {
        println!("[plant] {why}, draining up to 30s");
        let _ = shutdown_tx.send(true);
        let mut listener_leases = Vec::with_capacity(accept_loops.len());
        while let Some(result) = accept_loops.join_next().await {
            match result {
                Ok(listener) => listener_leases.push(listener),
                Err(error) => {
                    eprintln!("[plant] accept loop failed during shutdown: {error}");
                    record_exit(started, "exit:1");
                    std::process::exit(1);
                }
            }
        }
        drop(listener_leases);
        record_exit(started, "exit:0");
        std::process::exit(0);
    }
    record_exit(started, why);
    std::process::exit(128);
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match cli::parse_command(&argv) {
        Ok(command) => std::process::exit(dispatch(command).await),
        Err(error) => std::process::exit(usage(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_capture_schema_preserves_process_liveness() {
        for adapter in adapter::adapters() {
            let health = proxy::health_body_with_status(&adapter, 0, 0, Some(64), 64);
            assert_eq!(
                health,
                serde_json::json!({
                    "service": "plant",
                    "ok": true,
                    "capture_ok": true,
                    "harness": adapter.harness.capture_label(),
                    "upstream": adapter.upstream.trim_end_matches('/'),
                    "recorded_drops": 0,
                    "unrecorded_drops": 0,
                    "headroom_bytes": 64,
                    "headroom_floor": 64,
                })
            );
            assert!(health_matches(&health, &adapter));
        }
    }

    /// Stand up a real listener that serves a valid /health body, but only after
    /// `delay` — an incumbent that is alive and correct, just starved of CPU.
    /// Returns an adapter pointed at it.
    async fn starved_incumbent(delay: Duration) -> adapter::Adapter {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut serving = adapter::adapters().remove(0);
        serving.port = port;
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = proxy::health_body_with_status(&serving, 0, 0, Some(64), 64).to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    use tokio::io::AsyncWriteExt;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        let mut probed = adapter::adapters().remove(0);
        probed.port = port;
        probed
    }

    #[tokio::test]
    async fn slow_incumbent_under_load_is_not_mistaken_for_a_dead_one() {
        // 2.5s is past the 2s budget this probe used to carry and well inside the
        // current one — the exact window in which a loaded host crash-looped a
        // challenger against an incumbent that was serving 200s throughout.
        let adapter = starved_incumbent(Duration::from_millis(2_500)).await;

        assert!(
            incumbent_owns(&http_client(), &adapter, INCUMBENT_PROBE_TIMEOUT).await,
            "a healthy incumbent answering in 2.5s must be recognized as the owner"
        );
        // Negative control against the same live server: shrink only the budget
        // and the probe must fail, or the assertion above proves nothing.
        assert!(
            !incumbent_owns(&http_client(), &adapter, Duration::from_millis(200)).await,
            "a budget below the incumbent's response time must still read as incomplete"
        );
    }

    #[test]
    fn incumbent_probe_timeout_is_configurable() {
        std::env::remove_var("VAULTR_INCUMBENT_PROBE_TIMEOUT_MS");
        assert_eq!(incumbent_probe_timeout(), INCUMBENT_PROBE_TIMEOUT);

        std::env::set_var("VAULTR_INCUMBENT_PROBE_TIMEOUT_MS", "1234");
        assert_eq!(incumbent_probe_timeout(), Duration::from_millis(1234));
        std::env::remove_var("VAULTR_INCUMBENT_PROBE_TIMEOUT_MS");
    }

    #[tokio::test]
    async fn ownership_seam_requires_both_bindings_before_recovery_and_runtime() {
        let mut servers = Vec::new();
        let mut addresses = Vec::new();
        for adapter in adapter::adapters() {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            addresses.push(address);
            servers.push((listener, address.port(), adapter));
        }
        let ownership = ListenerOwnership { servers };
        let recovered = ownership
            .recover_with(|| {
                for address in &addresses {
                    assert!(
                        std::net::TcpListener::bind(address).is_err(),
                        "recovery runs only while both listener leases are held"
                    );
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            recovered.servers.len(),
            2,
            "only recovered dual ownership can enter proxy/scheduler startup"
        );
        drop(recovered);
    }
}
