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

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentCli {
    Claude,
    Codex,
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
fn usage(error: &str) -> i32 {
    eprintln!("plant: {error}");
    eprintln!(
        "usage: plant [--self-test | sessions eligible|coverage|stuck ... | \
         compress once ... | jobs run <name> | agent run --cli claude|codex ...]"
    );
    2
}

async fn subcommand(argv: &[String]) -> i32 {
    match argv.get(1).map(String::as_str) {
        Some("--self-test") if argv.len() == 2 => {
            selftest::self_test().await;
            0
        }
        Some("sessions") if argv.get(2).map(String::as_str) == Some("eligible") => {
            let mut learner = "claude";
            let mut idle = Duration::from_secs(3600);
            let mut max = 10;
            let mut claim = None;
            let mut seen = std::collections::HashSet::new();
            let mut i = 3;
            while i < argv.len() {
                let flag = argv[i].as_str();
                if !seen.insert(flag) {
                    return usage(&format!("duplicate sessions eligible flag {flag}"));
                }
                match flag {
                    "--learner" => {
                        let Some(value) = argv.get(i + 1).map(String::as_str) else {
                            return usage("sessions eligible: --learner requires claude|codex");
                        };
                        learner = match value {
                            "claude" => "claude",
                            "codex" => "codex",
                            _ => {
                                return usage("sessions eligible: --learner requires claude|codex");
                            }
                        };
                        i += 2;
                    }
                    "--idle" => {
                        let Some(value) = argv.get(i + 1) else {
                            return usage("sessions eligible: --idle requires a duration");
                        };
                        let Some(value) = jobs::parse_duration(value) else {
                            return usage("sessions eligible: invalid --idle duration");
                        };
                        idle = value;
                        i += 2;
                    }
                    "--max" => {
                        let Some(value) = argv.get(i + 1) else {
                            return usage("sessions eligible: --max requires an integer");
                        };
                        let Ok(value) = value.parse() else {
                            return usage("sessions eligible: invalid --max");
                        };
                        max = value;
                        i += 2;
                    }
                    "--claim" => {
                        claim = match argv.get(i + 1).map(String::as_str) {
                            Some(value) if !value.starts_with("--") => {
                                let Some(duration) = jobs::parse_duration(value) else {
                                    return usage("sessions eligible: invalid --claim duration");
                                };
                                i += 2;
                                Some(duration)
                            }
                            _ => {
                                i += 1;
                                Some(Duration::from_secs(50 * 60))
                            }
                        };
                    }
                    _ => return usage(&format!("unknown sessions eligible flag {flag}")),
                }
            }
            let vault = vault_root();
            let list = match claim {
                Some(lease) => {
                    let expires_at = jobs::epoch_now()
                        .saturating_add(lease.as_secs())
                        .saturating_add(300);
                    match sweep::eligible_and_claim(&vault, idle, max, learner, expires_at) {
                        Ok(list) => list,
                        Err(e) => {
                            eprintln!("sessions eligible: claim failed: {e}");
                            return 1;
                        }
                    }
                }
                None => sweep::eligible_sessions(&vault, idle, max, learner),
            };
            let (total, ledgered) = sweep::eligibility_stats(&vault, learner);
            eprintln!(
                "[eligible:{learner}] {} of {total} sessions ({ledgered} ledgered)",
                list.len()
            );
            if list.is_empty() {
                return 1;
            }
            println!("{}", list.join(" "));
            0
        }
        Some("sessions") if argv.get(2).map(String::as_str) == Some("coverage") => {
            if argv.len() != 4 {
                return usage("sessions coverage: exactly one session id is required");
            }
            match sweep::coverage(&vault_root(), &argv[3]) {
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
            }
        }
        Some("sessions") if argv.get(2).map(String::as_str) == Some("stuck") => {
            let age = match argv.get(3..).unwrap_or_default() {
                [] => Duration::from_secs(24 * 3600),
                [flag, value] if flag == "--age" => {
                    let Some(age) = jobs::parse_duration(value) else {
                        return usage("sessions stuck: invalid --age duration");
                    };
                    age
                }
                _ => return usage("sessions stuck: expected [--age <duration>]"),
            };
            let stuck = sweep::stuck_captures(&vault_root(), age);
            for s in &stuck {
                println!("{} {} idle={}h", s.state, s.sid, s.idle_secs / 3600);
            }
            println!("{}", sweep::stuck_summary(&stuck));
            // sub-threshold can never seal by design; job-capture is plant's own pane
            if stuck
                .iter()
                .any(|s| s.state != "sub-threshold" && s.state != "job-capture")
            {
                1
            } else {
                0
            }
        }
        Some("compress") if argv.get(2).map(String::as_str) == Some("once") => {
            let idle = match argv.get(3..).unwrap_or_default() {
                [] => Duration::from_secs(3600),
                [flag, value] if flag == "--idle" => {
                    let Some(idle) = jobs::parse_duration(value) else {
                        return usage("compress once: invalid --idle duration");
                    };
                    idle
                }
                _ => return usage("compress once: expected [--idle <duration>]"),
            };
            if sweep::compress_sweep(&vault_root(), idle).await {
                0
            } else {
                1
            }
        }
        Some("jobs") if argv.get(2).map(String::as_str) == Some("run") => {
            if argv.len() != 4 || argv[3].is_empty() {
                return usage("jobs run: exactly one job name is required");
            }
            let name = &argv[3];
            match jobs::load_jobs().into_iter().find(|j| j.name == *name) {
                Some(job) => jobs::run_job(&job).await,
                None => {
                    eprintln!(
                        "unknown job '{name}' (scripts: {})",
                        jobs::load_jobs()
                            .iter()
                            .map(|j| j.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    1
                }
            }
        }
        Some("agent") if argv.get(2).map(String::as_str) == Some("run") => {
            let mut cli = None;
            let mut model = None;
            let mut args = None;
            let mut label = None;
            let mut cwd = None;
            let mut timeout = Duration::from_secs(45 * 60);
            let mut requested = herdr::WorkspaceCleanup::Never;
            let mut seen = std::collections::HashSet::new();
            let mut i = 3;
            while i < argv.len() {
                let flag = argv[i].as_str();
                if !seen.insert(flag) {
                    return usage(&format!("duplicate agent run flag {flag}"));
                }
                let Some(value) = argv.get(i + 1).map(String::as_str) else {
                    return usage(&format!("agent run: {flag} requires a value"));
                };
                match flag {
                    "--cli" => {
                        cli = Some(match value {
                            "claude" => AgentCli::Claude,
                            "codex" => AgentCli::Codex,
                            _ => return usage("agent run: --cli requires claude|codex"),
                        });
                    }
                    "--model" => model = Some(value),
                    "--args" => args = Some(value),
                    "--label" => label = Some(value),
                    "--cwd" => cwd = Some(value),
                    "--timeout" => {
                        let Some(value) = jobs::parse_duration(value) else {
                            return usage("agent run: invalid --timeout duration");
                        };
                        timeout = value;
                    }
                    "--cleanup" => {
                        requested = match value {
                            "always" => herdr::WorkspaceCleanup::Always,
                            "on-success" => herdr::WorkspaceCleanup::OnSuccess,
                            "never" => herdr::WorkspaceCleanup::Never,
                            _ => {
                                return usage(
                                    "agent run: --cleanup requires always|on-success|never",
                                );
                            }
                        };
                    }
                    _ => return usage(&format!("unknown agent run flag {flag}")),
                }
                i += 2;
            }
            let Some(cli) = cli else {
                return usage("agent run: --cli claude|codex is required");
            };
            let mut prompt = String::new();
            use std::io::Read;
            if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.trim().is_empty() {
                eprintln!("agent run: prompt expected on stdin");
                return 1;
            }
            let (cli_name, discover_session_id) = match cli {
                AgentCli::Claude => ("claude", false),
                AgentCli::Codex => ("codex", true),
            };
            // Keep learn passes from dispatching on this pane's own capture (a
            // learn-over-learn run per learner, per capture). Claude sids are ours to
            // mint, so preset + register before launch. Codex assigns conversation ids
            // server-side, so run_agent discovers and registers the id herdr reports for
            // the pane once the run finishes (discover_session_id below).
            let mut launch = jobs::launch_line(cli_name, model, args);
            if cli == AgentCli::Claude {
                let sid = uuid::Uuid::new_v4().to_string();
                sweep::register_job_sid(&sid);
                launch.push_str(&format!(" --session-id '{sid}'"));
            }
            let vault = vault_root();
            let run = herdr::AgentRun {
                label: label.unwrap_or("agent").to_string(),
                cwd: cwd.map(String::from).unwrap_or_else(|| {
                    format!("{}/.dotfiles", std::env::var("HOME").unwrap_or_default())
                }),
                launch,
                prompt: prompt.trim().to_string(),
                timeout,
                cleanup: jobs::cleanup_policy(requested, &jobs::Cfg::load(&vault)),
                discover_session_id,
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
        _ => usage("unrecognized command"),
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
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() > 1 {
        std::process::exit(subcommand(&argv).await);
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

    // Recover ordered-capture journals and staged Envelopes BEFORE binding ports
    // or arming the scheduler (which may seal). Failing closed preserves evidence.
    if let Err(e) = capture::recover_all(&vault) {
        eprintln!("[plant] capture recovery failed: {e}");
        record_exit(started, "exit:1");
        std::process::exit(1);
    }

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
