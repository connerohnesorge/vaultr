//! plant — Rust rewrite of wireproxy (Bun). Same behavioral contract:
//! see ~/.dotfiles/.claude/plans/handoff-redesign-wireproxy-linked-mountain.md
//! and the original ~/.dotfiles/.config/wireproxy/wireproxy.ts.

mod adapter;
mod capture;
mod herdr;
mod jobs;
mod otel;
mod proxy;
mod selftest;
mod sweep;

use proxy::ProxyCtx;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn crash_log() -> PathBuf {
    let dir = jobs::state_dir();
    let _ = std::fs::create_dir_all(&dir);
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
/// with the prompt on stdin — the ONLY sanctioned way for job scripts to drive an agent
/// (Herdr pane orchestration; never `claude -p`). Exit: 0 succeeded, 75 herdr
/// unavailable (retry later), 1 failed. Requested --cleanup is honored only when
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
            Some(if sweep::compress_sweep(&vault_root(), idle).await {
                0
            } else {
                1
            })
        }
        (Some("jobs"), Some("run")) => {
            let name = argv.get(3).cloned().unwrap_or_default();
            match jobs::load_jobs().into_iter().find(|j| j.name == name) {
                Some(job) => {
                    jobs::run_job(&job).await;
                    Some(0)
                }
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
            // Pre-register the claude session id so learn passes never dispatch on this
            // pane's own capture (a learn-over-learn run per learner, per capture).
            // Codex assigns conversation ids server-side — nothing to register.
            let mut launch =
                jobs::launch_line(&cli, flag("--model").as_deref(), flag("--args").as_deref());
            if cli == "claude" {
                let sid = uuid::Uuid::new_v4().to_string();
                sweep::register_job_sid(&sid);
                launch.push_str(&format!(" --session-id '{sid}'"));
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
            };
            let label = run.label.clone();
            match herdr::run_agent(run).await {
                herdr::AgentRunOutcome::Succeeded(detail) => {
                    println!("[agent:{label}] succeeded: {detail}");
                    Some(0)
                }
                herdr::AgentRunOutcome::Unavailable => {
                    println!("[agent:{label}] herdr unavailable");
                    Some(75)
                }
                herdr::AgentRunOutcome::Failed(detail) => {
                    println!("[agent:{label}] failed: {detail}");
                    Some(1)
                }
            }
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

    let otel = Arc::new(otel::Otel::new());
    let client = http_client();

    // Bind both ports first: collision => yield to the live instance (exit 0).
    let mut servers = vec![];
    for a in adapter::adapters() {
        match proxy::bind(a.port).await {
            Ok((listener, port)) => {
                println!("vaultr [{}] 127.0.0.1:{port} -> {}", a.harness, a.upstream);
                servers.push((listener, a));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                println!("[vaultr] port already bound, another instance owns it — exiting 0");
                record_exit(started, "exit:0");
                std::process::exit(0);
            }
            Err(e) => {
                record_exit(started, "exit:1");
                panic!("bind failed: {e}");
            }
        }
    }
    println!("vault={}", vault.display());

    let mut accept_loops = vec![];
    for (listener, a) in servers {
        let ctx = Arc::new(ProxyCtx {
            adapter: a,
            vault: vault.clone(),
            client: client.clone(),
            otel: otel.clone(),
        });
        accept_loops.push(tokio::spawn(proxy::serve(listener, ctx)));
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

    tokio::spawn(jobs::scheduler(jobs::Cfg::load(&vault)));

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
        // drop listeners first so a replacement can bind immediately; per-connection
        // tasks are independent spawns, so in-flight streams still get the 30s drain
        for h in &accept_loops {
            h.abort();
        }
        println!("[plant] {why}, draining up to 30s");
        tokio::time::sleep(Duration::from_secs(30)).await;
        record_exit(started, "exit:0");
        std::process::exit(0);
    }
    record_exit(started, why);
    std::process::exit(128);
}
