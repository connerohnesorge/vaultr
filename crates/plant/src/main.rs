//! plant — Rust rewrite of wireproxy (Bun). Same behavioral contract:
//! see ~/.dotfiles/.claude/plans/handoff-redesign-wireproxy-linked-mountain.md
//! and the original ~/.dotfiles/.config/wireproxy/wireproxy.ts.

mod adapter;
mod capture;
mod cli;
mod fsutil;
mod herdr;
mod jobs;
mod otel;
mod process;
mod proxy;
mod selftest;
mod sweep;

use cli::{AgentCli, Command};
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
fn vault_root() -> PathBuf {
    vaultr::vault::root(None).unwrap_or_else(|e| {
        eprintln!("[plant] cannot resolve vault sessions root: {e}");
        std::process::exit(1);
    })
}

fn usage(error: &str) -> i32 {
    eprintln!("plant: {error}");
    eprintln!(
        "usage: plant [--self-test | sessions eligible|coverage|stuck ... | \
         compress once ... | jobs run <name> | agent run --cli claude|codex ...]"
    );
    2
}

async fn dispatch(command: Command) -> i32 {
    match command {
        Command::Daemon => {
            run_daemon().await;
            0
        }
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
                    Err(e) => {
                        eprintln!("sessions eligible: claim failed: {e}");
                        return 1;
                    }
                },
                None => sweep::eligible_sessions(&vault, args.idle, args.max, args.learner),
            };
            let (total, ledgered) = sweep::eligibility_stats(&vault, args.learner);
            eprintln!(
                "[eligible:{}] {} of {total} sessions ({ledgered} ledgered)",
                args.learner,
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
        Command::SessionsCoverage(sid) => match sweep::coverage(&vault_root(), &sid) {
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
                    println!("  captured={} (>= in-window native)", c.captured);
                }
                if c.missing.is_empty() {
                    println!("  no in-window gap");
                    0
                } else {
                    println!("  missing {} in-window request-id(s):", c.missing.len());
                    for rid in &c.missing {
                        println!("    {rid}");
                    }
                    1
                }
            }
            Err(e) => {
                eprintln!("sessions coverage: {e}");
                1
            }
        },
        Command::SessionsStuck(age) => {
            let stuck = sweep::stuck_captures(&vault_root(), age);
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
        Command::CompressOnce(idle) => i32::from(!sweep::compress_sweep(&vault_root(), idle).await),
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
        Command::AgentRun(args) => {
            let mut prompt = String::new();
            use std::io::Read;
            if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
                eprintln!("agent run: prompt expected on stdin");
                return 1;
            }
            let mut launch = jobs::launch_line(
                args.cli.as_str(),
                args.model.as_deref(),
                args.args.as_deref(),
            );
            if args.cli == AgentCli::Claude {
                let sid = uuid::Uuid::new_v4().to_string();
                sweep::register_job_sid(&sid);
                launch.push_str(&format!(" --session-id '{sid}'"));
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
                discover_session_id: args.cli == AgentCli::Codex,
            };
            let label = run.label.clone();
            match herdr::run_agent(run).await {
                herdr::AgentRunOutcome::Succeeded(detail) => {
                    println!("[agent:{label}] succeeded: {detail}");
                    0
                }
                herdr::AgentRunOutcome::Unavailable => {
                    println!("[agent:{label}] herdr unavailable");
                    75
                }
                herdr::AgentRunOutcome::Failed(detail) => {
                    println!("[agent:{label}] failed: {detail}");
                    1
                }
            }
        }
    }
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

async fn run_daemon() {
    let started = SystemTime::now();

    std::panic::set_hook(Box::new(move |info| {
        let msg = format!(
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
            let _ = file.write_all(msg.as_bytes());
        }
        eprintln!("[plant] {info}");
        record_exit(started, "panic");
        std::process::exit(1);
    }));

    let vault = vault_root();
    if let Err(e) = capture::recover_all(&vault) {
        eprintln!("[plant] capture recovery failed: {e}");
        record_exit(started, "exit:1");
        std::process::exit(1);
    }

    let otel = Arc::new(otel::Otel::new());
    let client = http_client();
    let mut servers = vec![];
    for adapter in adapter::adapters() {
        match proxy::bind(adapter.port).await {
            Ok((listener, port)) => {
                println!(
                    "vaultr [{}] 127.0.0.1:{port} -> {}",
                    adapter.harness, adapter.upstream
                );
                servers.push((listener, adapter));
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
    for (listener, adapter) in servers {
        let ctx = Arc::new(ProxyCtx {
            adapter,
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
        for handle in &accept_loops {
            handle.abort();
        }
        println!("[plant] {why}, draining up to 30s");
        tokio::time::sleep(Duration::from_secs(30)).await;
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
