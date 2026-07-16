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

/// `plant sessions eligible [--learner claude|codex] [--idle 60m] [--max 10]`,
/// `plant compress once [--idle 60m]`, and `plant jobs run <name>` (manual trigger of a
/// built-in job; the Herdr workspace stays open after the run unless PLANT_KEEP_PANES=0
/// opts into the per-job cleanup policy — see jobs::cleanup_policy).
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
            println!("{}", list.join(" "));
            Some(0)
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
                    jobs::run_job(&job, &jobs::Cfg::load(&vault_path())).await;
                    Some(0)
                }
                None => {
                    eprintln!("unknown job '{name}'");
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
