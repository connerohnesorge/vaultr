//! plant — Rust rewrite of wireproxy (Bun). Same behavioral contract:
//! see ~/.dotfiles/.claude/plans/handoff-redesign-wireproxy-linked-mountain.md
//! and the original ~/.dotfiles/.config/wireproxy/wireproxy.ts.

mod adapter;
mod agent_run;
mod capture;
mod herdr;
mod jobs;
mod otel;
mod process;
mod proxy;
mod selftest;
mod state;
mod sweep;

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
/// Resolution only fails when both env vars are unset; main resolves this before serving, so
/// a failure here is a startup misconfiguration — exit loudly rather than capture to a wrong root.
fn vault_root() -> PathBuf {
    vaultr::vault::root(None).unwrap_or_else(|e| {
        eprintln!("[plant] cannot resolve vault sessions root: {e}");
        std::process::exit(1);
    })
}

/// `plant sessions eligible [--learner claude|codex] [--idle 60m] [--max 10] [--claim [50m]]`
/// (--claim leases the printed batch in-flight so a concurrent call can't re-dispatch it),
/// `plant sessions stuck [--age 24h]` (stuck-capture report; exit 1 on actionable
/// findings), `plant compress once [--idle 60m]`, `plant jobs run <name>` (manual
/// trigger of a vault/jobs script), and `plant agent run --cli claude|codex [--model M]
/// [--args '…'] [--label L] [--cleanup always|on-success|never] [--timeout 45m] [--cwd D]`
/// [--idempotency-key K]
/// with the prompt on stdin — the ONLY sanctioned way for job scripts to drive an agent
/// (Herdr pane orchestration; never `claude -p`). The final stdout line is a
/// machine-readable `plant.agent.run` result. Exit: 0 succeeded, 75 retryable,
/// 1 failed or indeterminate. Requested --cleanup is honored only when
/// PLANT_KEEP_PANES=0 opts in — see jobs::cleanup_policy.
async fn subcommand(argv: &[String]) -> Option<i32> {
    let flag = |name: &str| {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1).cloned())
    };
    let idle = flag("--idle")
        .and_then(|v| jobs::parse_duration(&v))
        .unwrap_or(Duration::from_secs(3600));
    match (
        argv.get(1).map(String::as_str),
        argv.get(2).map(String::as_str),
    ) {
        (Some("sessions"), Some("eligible")) => {
            let max = flag("--max").and_then(|v| v.parse().ok()).unwrap_or(10);
            let learner = flag("--learner").unwrap_or_else(|| "claude".to_string());
            let list = sweep::eligible_sessions(&vault_root(), idle, max, &learner);
            let (total, ledgered) = sweep::eligibility_stats(&vault_root(), &learner);
            eprintln!(
                "[eligible:{learner}] {} of {total} sessions ({ledgered} ledgered)",
                list.len()
            );
            if list.is_empty() {
                return Some(1);
            }
            if let Some(i) = argv.iter().position(|a| a == "--claim") {
                let lease = argv
                    .get(i + 1)
                    .and_then(|v| jobs::parse_duration(v))
                    .unwrap_or(Duration::from_secs(50 * 60));
                let sids: Vec<String> = list
                    .iter()
                    .filter_map(|d| {
                        PathBuf::from(d)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(String::from)
                    })
                    .collect();
                // same slack the old in-Rust scheduler added past the job timeout
                let expires_at = jobs::epoch_now() + lease.as_secs() + 300;
                sweep::claim_inflight(&vault_root(), &learner, &sids, expires_at);
            }
            println!("{}", list.join(" "));
            Some(0)
        }
        (Some("sessions"), Some("coverage")) => {
            let Some(sid) = argv.get(3) else {
                eprintln!("sessions coverage: <session-id> required");
                return Some(1);
            };
            match sweep::coverage(&vault_root(), sid) {
                Ok(c) => {
                    let tag = if c.resumed { " (resumed)" } else { "" };
                    println!(
                        "{} coverage {:.1}% ({}/{} in-window){tag}",
                        c.sid,
                        c.pct(),
                        c.in_window_native - c.missing.len(),
                        c.in_window_native,
                    );
                    println!(
                        "  window_start={} carryover={}",
                        c.window_start, c.carryover
                    );
                    if c.captured > c.in_window_native {
                        // captured envelopes with no in-window native match (e.g. pre-window
                        // boundary or non-transcript calls) — informational, not loss.
                        println!("  captured={} (>= in-window native)", c.captured);
                    }
                    if c.missing.is_empty() {
                        println!("  no in-window gap");
                        Some(0)
                    } else {
                        println!("  missing {} in-window request-id(s):", c.missing.len());
                        for rid in &c.missing {
                            println!("    {rid}");
                        }
                        Some(1)
                    }
                }
                Err(e) => {
                    eprintln!("sessions coverage: {e}");
                    Some(1)
                }
            }
        }
        (Some("sessions"), Some("stuck")) => {
            let age = flag("--age")
                .and_then(|v| jobs::parse_duration(&v))
                .unwrap_or(Duration::from_secs(24 * 3600));
            let stuck = sweep::stuck_captures(&vault_root(), age);
            for s in &stuck {
                println!("{} {} idle={}h", s.state, s.sid, s.idle_secs / 3600);
            }
            // sub-threshold can never seal by design; job-capture is plant's own pane
            Some(
                if stuck
                    .iter()
                    .any(|s| s.state != "sub-threshold" && s.state != "job-capture")
                {
                    1
                } else {
                    0
                },
            )
        }
        (Some("compress"), Some("once")) => {
            // Compression may mutate capture files only while this process owns
            // both listener bindings. Retain the listeners through recovery and
            // the full sweep so no daemon can capture concurrently.
            let ownership = match ListenerOwnership::bind_all().await {
                Ok(ownership) => ownership,
                Err(error) => {
                    eprintln!("compress once: listener ownership unavailable: {error}");
                    return Some(2);
                }
            };
            let vault = vault_root();
            let _ownership = match ownership.recover(&vault) {
                Ok(ownership) => ownership,
                Err(error) => {
                    eprintln!("compress once: capture recovery failed: {error}");
                    return Some(2);
                }
            };
            Some(if sweep::compress_sweep(&vault, idle).await {
                0
            } else {
                2
            })
        }
        (Some("jobs"), Some("run")) => {
            let name = argv.get(3).cloned().unwrap_or_default();
            match jobs::load_jobs().into_iter().find(|j| j.name == name) {
                Some(job) => Some(jobs::run_job(&job).await),
                None => {
                    eprintln!(
                        "unknown job '{name}' (scripts: {})",
                        jobs::load_jobs()
                            .iter()
                            .map(|j| j.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    Some(1)
                }
            }
        }
        (Some("agent"), Some("run")) => {
            let Some(cli) = flag("--cli") else {
                eprintln!("agent run: --cli claude|codex is required");
                return Some(1);
            };
            let mut prompt = String::new();
            use std::io::Read;
            if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
                eprintln!("agent run: prompt expected on stdin");
                return Some(1);
            }
            let requested = match flag("--cleanup").as_deref() {
                Some("always") => herdr::WorkspaceCleanup::Always,
                Some("on-success") => herdr::WorkspaceCleanup::OnSuccess,
                _ => herdr::WorkspaceCleanup::Never,
            };
            // Keep learn passes from dispatching on this pane's own capture (a
            // learn-over-learn run per learner, per capture). Claude sids are ours to
            // mint, so preset + register before launch. Codex assigns conversation ids
            // server-side, so run_agent discovers and registers the id herdr reports for
            // the pane once the run finishes (discover_session_id below).
            let mut launch =
                jobs::launch_line(&cli, flag("--model").as_deref(), flag("--args").as_deref());
            let mut preset_session_id = None;
            if cli == "claude" {
                let sid = uuid::Uuid::new_v4().to_string();
                launch.push_str(&format!(" --session-id '{sid}'"));
                preset_session_id = Some(sid);
            }
            let run = herdr::AgentRun {
                label: flag("--label").unwrap_or_else(|| "agent".to_string()),
                cwd: flag("--cwd").unwrap_or_else(|| {
                    format!("{}/.dotfiles", std::env::var("HOME").unwrap_or_default())
                }),
                launch,
                prompt: prompt.trim().to_string(),
                timeout: flag("--timeout")
                    .and_then(|v| jobs::parse_duration(&v))
                    .unwrap_or(Duration::from_secs(45 * 60)),
                cleanup: jobs::cleanup_policy(requested, &jobs::Cfg::load(&vault_root())),
                preset_session_id,
                discover_session_id: cli == "codex",
            };
            let receipt = match flag("--idempotency-key") {
                Some(key) => agent_run::run_idempotent(run, &key).await,
                None => agent_run::AgentRunReceipt::untracked(herdr::run_agent(run).await),
            };
            println!(
                "{}",
                serde_json::to_string(&receipt).expect("receipt serializes")
            );
            Some(receipt.exit_code())
        }
        _ => None,
    }
}

fn rss_mb() -> u64 {
    // ps reports KB; death path, shelling out is fine
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn record_exit(started: SystemTime, why: &str) {
    let up_s = started.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    let line = format!(
        "\n----- exit {} pid={} why={why} rss={}MB uptime={up_s}s -----\n",
        capture::iso_now(),
        std::process::id(),
        rss_mb(),
    );
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crash_log())
    {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("reqwest client")
}

async fn complete_incumbent() -> bool {
    let client = http_client();
    for adapter in adapter::adapters() {
        let url = format!("http://127.0.0.1:{}/health", adapter.port);
        let response =
            match tokio::time::timeout(Duration::from_secs(2), client.get(url).send()).await {
                Ok(Ok(response)) if response.status().is_success() => response,
                _ => return false,
            };
        let Ok(health) = response.json::<serde_json::Value>().await else {
            return false;
        };
        if !health_matches(&health, &adapter) {
            return false;
        }
    }
    true
}

fn health_matches(health: &serde_json::Value, adapter: &adapter::Adapter) -> bool {
    health.get("service").and_then(|value| value.as_str()) == Some("plant")
        && health.get("ok").and_then(|value| value.as_bool()) == Some(true)
        && health.get("harness").and_then(|value| value.as_str()) == Some(adapter.harness)
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

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        selftest::self_test().await;
        return;
    }
    let argv: Vec<String> = std::env::args().collect();
    if let Some(code) = subcommand(&argv).await {
        std::process::exit(code);
    }

    let started = SystemTime::now();

    // Death recorder: crash.log only catches panics; signals and clean exits left no
    // trace, so an intermittent death was unprovable. SIGKILL/OOM stay uncatchable by
    // design — absence of a line = hard kill. RSS at death distinguishes OOM-adjacent.
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!(
            "\n===== panic {} pid={} =====\n{info}\n",
            capture::iso_now(),
            std::process::id()
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crash_log())
        {
            let _ = f.write_all(msg.as_bytes());
        }
        eprintln!("[plant] {info}");
        record_exit(started, "panic");
        std::process::exit(1);
    }));

    let vault = vault_root();

    // Own both harness listeners before recovery or scheduler work. A partial
    // bind is dropped before deciding whether a complete daemon is incumbent.
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
            adapter.harness, adapter.upstream
        );
    }

    // With both capture endpoints unavailable to any competitor, recover all
    // ordered journals and staged envelopes before scheduler work can seal.
    let ownership = ownership.recover(&vault).unwrap_or_else(|error| {
        eprintln!("[plant] capture recovery failed: {error}");
        record_exit(started, "exit:1");
        std::process::exit(1);
    });

    let otel = Arc::new(otel::Otel::new());
    let client = http_client();
    println!("vault={}", vault.display());

    let mut _accept_loops = vec![];
    for (listener, _, a) in ownership.into_servers() {
        let ctx = Arc::new(ProxyCtx {
            adapter: a,
            vault: vault.clone(),
            client: client.clone(),
            otel: otel.clone(),
        });
        _accept_loops.push(tokio::spawn(proxy::serve(listener, ctx)));
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

    tokio::spawn(jobs::scheduler(jobs::Cfg::load(&vault), vault.clone()));

    // hourly RSS breadcrumb for the leak investigation
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            println!("[plant] rss={}MB", rss_mb());
        }
    });

    // Signals: SIGTERM/SIGINT drain (in-flight streams finish; hard exit after 30s).
    // SIGHUP/SIGQUIT/SIGUSR1/SIGUSR2 recorded then exit 128 — the death recorder.
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
    if why == "signal:SIGTERM" || why == "signal:SIGINT" {
        // Retain both listener leases through the bounded drain. Releasing them
        // early would let a replacement recover/seal while existing capture
        // tasks can still commit.
        println!("[plant] {why}, draining up to 30s");
        tokio::time::sleep(Duration::from_secs(30)).await;
        record_exit(started, "exit:0");
        std::process::exit(0);
    }
    record_exit(started, why);
    std::process::exit(128);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incumbent_health_schema_matches_the_proxy_contract_exactly() {
        for adapter in adapter::adapters() {
            let health = proxy::health_body(&adapter);
            assert_eq!(
                health,
                serde_json::json!({
                    "service": "plant",
                    "ok": true,
                    "harness": adapter.harness,
                    "upstream": adapter.upstream.trim_end_matches('/'),
                })
            );
            assert!(health_matches(&health, &adapter));
        }
    }

    #[tokio::test]
    async fn listener_lease_survives_the_bounded_drain() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let drain = tokio::spawn(async move {
            let _lease = listener;
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        assert!(
            tokio::net::TcpListener::bind(address).await.is_err(),
            "replacement must not bind during drain"
        );
        drain.await.unwrap();
        let replacement = tokio::net::TcpListener::bind(address).await.unwrap();
        drop(replacement);
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
