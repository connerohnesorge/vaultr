//! Built-in jobs, defined as Rust code in load_jobs()/run_job() below. Compress calls
//! the sweep directly in-process; learn/learn-codex/reconcile compute a prompt in Rust
//! and type it into a fresh Herdr pane.
//! `close_pane` (Always|OnSuccess) controls whether the job's Herdr workspace is
//! closed when the run ends; kept workspaces are reclaimed by the next run of the same job.
//! Outcomes append to ~/.local/state/plant/jobs/<name>.jsonl; the tail line is the
//! scheduling state (due when now - last.ts >= every). Never `claude -p`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sweep::{run, run30};

pub fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state/plant")
}

/// Recognized config keys from real env then vault/.env — a private map, never
/// std::env::set_var: nothing here may leak into child process environments.
pub struct Cfg(HashMap<String, String>);

impl Cfg {
    pub fn load(vault_sessions: &Path) -> Self {
        let mut map = HashMap::new();
        if let Some(f) = vault_sessions.parent().map(|v| v.join(".env")) {
            if let Ok(text) = std::fs::read_to_string(f) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        map.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }
        }
        Cfg(map)
    }

    pub fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().or_else(|| self.0.get(key).cloned())
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ClosePane {
    Always,
    OnSuccess,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Compress,
    Learn,
    LearnCodex,
    Reconcile,
}

#[derive(Clone)]
pub struct Job {
    pub name: String,
    pub kind: Kind,
    pub every: Duration,
    pub timeout: Duration,
    pub cli: Option<String>,
    pub model: Option<String>,
    pub args: Option<String>,
    pub cwd: String,
    pub close_pane: ClosePane,
}

/// "90s" / "15m" / "2h" / "30d" — single number + unit.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.len() < 2 || !s.is_ascii() {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

fn expand_home(v: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if v == "~" {
        home
    } else if let Some(rest) = v.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        v.to_string()
    }
}

impl Job {
    fn new(name: &str, kind: Kind, every: Duration) -> Job {
        Job {
            name: name.to_string(),
            kind,
            every,
            timeout: Duration::from_secs(45 * 60),
            cli: None,
            model: None,
            args: None,
            cwd: expand_home("~/.dotfiles"),
            close_pane: ClosePane::Always,
        }
    }
}

const MIN: u64 = 60;

pub fn load_jobs() -> Vec<Job> {
    vec![
        Job {
            timeout: Duration::from_secs(2 * 3600),
            ..Job::new("compress", Kind::Compress, Duration::from_secs(30 * MIN))
        },
        Job {
            cli: Some("claude".into()),
            model: Some("opus[1m]".into()),
            close_pane: ClosePane::OnSuccess,
            ..Job::new("learn", Kind::Learn, Duration::from_secs(15 * MIN))
        },
        Job {
            cli: Some("codex".into()),
            model: Some("gpt-5.6-sol".into()),
            args: Some("-c model_reasoning_effort=xhigh".into()),
            close_pane: ClosePane::OnSuccess,
            ..Job::new(
                "learn-codex",
                Kind::LearnCodex,
                Duration::from_secs(15 * MIN),
            )
        },
        Job {
            cli: Some("claude".into()),
            model: Some("opus[1m]".into()),
            ..Job::new(
                "reconcile",
                Kind::Reconcile,
                Duration::from_secs(30 * 86400),
            )
        },
    ]
}

/// Eligible session dirs for a learner, or None when there's nothing to learn from.
fn eligible(learner: &str) -> Option<String> {
    let vault = crate::vault_path();
    let list = crate::sweep::eligible_sessions(&vault, Duration::from_secs(3600), 10, learner);
    let (total, ledgered) = crate::sweep::eligibility_stats(&vault, learner);
    println!(
        "[eligible:{learner}] {} of {total} sessions ({ledgered} ledgered)",
        list.len()
    );
    (!list.is_empty()).then(|| list.join(" "))
}

/// The prompt an agent job types into its Herdr pane; None => nothing to do, skip.
fn prompt(kind: Kind) -> Option<String> {
    match kind {
        Kind::Compress => None,
        Kind::Learn => {
            eligible("claude").map(|dirs| format!("/Vault learn --learner claude {dirs}"))
        }
        Kind::LearnCodex => eligible("codex").map(|dirs| {
            format!(
                "$Vault Learn with `--learner codex` and these session directories as input: {dirs}"
            )
        }),
        Kind::Reconcile => Some("/Vault reconcile".to_string()),
    }
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn last_record_ts(name: &str) -> Option<u64> {
    let text =
        std::fs::read_to_string(state_dir().join("jobs").join(format!("{name}.jsonl"))).ok()?;
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("ts")?
        .as_u64()
}

fn record(name: &str, outcome: &str, started: SystemTime, detail: &str) {
    let dir = state_dir().join("jobs");
    let _ = std::fs::create_dir_all(&dir);
    let rec = serde_json::json!({
        "ts": epoch_now(),
        "iso": crate::capture::iso_now(),
        "outcome": outcome,
        "duration_ms": started.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0),
        "detail": detail,
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{name}.jsonl")))
    {
        let _ = writeln!(f, "{rec}");
    }
    println!("[job:{name}] {outcome} ({detail})");
}

fn launch_line(job: &Job) -> String {
    let codex = job.cli.as_deref() == Some("codex");
    // `command` bypasses the user's interactive-shell aliases (a `codex='codex --yolo'`
    // alias duplicated our flag, clap refused, and the prompt got typed into bare zsh)
    let mut s = if codex {
        // sandboxed codex blocks on its first approval prompt — background panes can't answer
        "command codex --dangerously-bypass-approvals-and-sandbox".to_string()
    } else {
        "command claude --dangerously-skip-permissions".to_string()
    };
    if let Some(m) = &job.model {
        s.push_str(&if codex {
            format!(" -m '{m}'")
        } else {
            format!(" --model='{m}'")
        });
    }
    if let Some(a) = &job.args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

/// herdr's agent-status reports "idle" for a bare shell too — a failed CLI launch drops
/// the pane back to zsh and typing a prompt there executes it as shell commands. Only
/// trust a pane that herdr has actually recognized as running an agent.
async fn pane_has_agent(pane: &str) -> bool {
    let list = run30(&["herdr", "pane", "list"]).await;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&list.out) else {
        return false;
    };
    v.pointer("/result/panes")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .any(|p| {
            p.get("pane_id").and_then(|i| i.as_str()) == Some(pane)
                && p.get("agent").and_then(|a| a.as_str()).is_some()
        })
}

async fn focused_workspace() -> Option<String> {
    let list = run30(&["herdr", "workspace", "list"]).await;
    let v: serde_json::Value = serde_json::from_str(&list.out).ok()?;
    v.pointer("/result/workspaces")?
        .as_array()?
        .iter()
        .find(|w| w.get("focused").and_then(|f| f.as_bool()).unwrap_or(false))?
        .get("workspace_id")?
        .as_str()
        .map(String::from)
}

/// herdr `workspace close` refocuses the newest remaining workspace even when the
/// closed one wasn't focused (upstream bug) — put the user's focus back if it moved.
async fn close_workspace(id: &str) {
    let before = focused_workspace().await;
    run30(&["herdr", "workspace", "close", id]).await;
    if let Some(prev) = before {
        if prev != id && focused_workspace().await.as_deref() != Some(prev.as_str()) {
            run30(&["herdr", "workspace", "focus", &prev]).await;
        }
    }
}

/// Close any workspace labeled `job-<name>` — recovery for the case where workspace
/// create returned unparseable output but did create one. Returns how many it closed;
/// >0 means the create "failure" actually leaked a live workspace.
async fn close_by_label(label: &str) -> u32 {
    let list = run30(&["herdr", "workspace", "list"]).await;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&list.out) else {
        return 0;
    };
    let mut closed = 0;
    for ws in v
        .pointer("/result/workspaces")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
    {
        if ws.get("label").and_then(|l| l.as_str()) == Some(label) {
            // a kept pane from a falsely-"failed" run can still hold a working agent —
            // never close those out from under it
            if ws.get("agent_status").and_then(|s| s.as_str()) == Some("working") {
                continue;
            }
            if let Some(id) = ws.get("workspace_id").and_then(|i| i.as_str()) {
                close_workspace(id).await;
                closed += 1;
            }
        }
    }
    closed
}

/// Fresh Herdr workspace -> launch CLI -> wait idle -> type rendered body -> wait done.
/// None => herdr down (records nothing; next tick retries). Some((ok, detail)) => real attempt.
async fn run_agent(job: &Job, rendered: &str) -> Option<(bool, String)> {
    if !run30(&["herdr", "workspace", "list"]).await.ok {
        return None;
    }
    let label = format!("job-{}", job.name);
    // reclaim workspaces from prior runs: SIGTERM'd mid-run leaks, or close-pane kept ones
    let stale = close_by_label(&label).await;
    if stale > 0 {
        println!("[job:{}] reclaimed {stale} stale workspace(s)", job.name);
    }
    let ws = run30(&[
        "herdr",
        "workspace",
        "create",
        "--cwd",
        &job.cwd,
        "--label",
        &label,
        "--no-focus",
    ])
    .await;
    let (mut ws_id, mut pane) = (None::<String>, None::<String>);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ws.out) {
        ws_id = v
            .pointer("/result/workspace/workspace_id")
            .and_then(|s| s.as_str())
            .map(String::from);
        pane = v
            .pointer("/result/root_pane/pane_id")
            .and_then(|s| s.as_str())
            .map(String::from);
    }
    let result = async {
        let (Some(pane), Some(_)) = (&pane, &ws_id) else {
            eprintln!(
                "[job:{}] workspace create failed: {}",
                job.name,
                ws.out.chars().take(200).collect::<String>()
            );
            let orphans = close_by_label(&label).await;
            let detail = if orphans > 0 {
                format!("workspace create unparseable, {orphans} orphan(s) closed by label")
            } else {
                "workspace create failed".to_string()
            };
            return Some((false, detail));
        };
        if !run30(&["herdr", "pane", "run", pane, &launch_line(job)])
            .await
            .ok
        {
            return Some((false, "pane run failed".to_string()));
        }
        if !run(
            &[
                "herdr",
                "wait",
                "agent-status",
                pane,
                "--status",
                "idle",
                "--timeout",
                "60000",
            ],
            Duration::from_secs(70),
        )
        .await
        .ok
        {
            eprintln!(
                "[job:{}] agent did not become ready in pane {pane}",
                job.name
            );
            return Some((false, "agent never became ready".to_string()));
        }
        // agent-status flips idle on process detection, ~1-2s BEFORE the TUI reads input —
        // a paste sent in that window is swallowed whole. Wait out startup, then verify
        // the text actually landed in the pane before submitting; retype if it didn't.
        tokio::time::sleep(Duration::from_secs(3)).await;
        if !pane_has_agent(pane).await {
            return Some((false, "CLI never launched (pane has no agent)".to_string()));
        }
        let needle: String = rendered.chars().take(30).collect();
        let mut landed = false;
        for _ in 0..3 {
            run30(&["herdr", "pane", "run", pane, rendered]).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            let read = run30(&[
                "herdr",
                "pane",
                "read",
                pane,
                "--source",
                "recent-unwrapped",
                "--lines",
                "80",
            ])
            .await;
            if !read.ok {
                break; // can't verify — don't blind-retype; fail and retry next tick
            }
            // claude's TUI collapses large pastes to "[Pasted text #N]" — that placeholder
            // IS the prompt having landed; retyping on it queues duplicate prompts
            if read.out.contains(&needle) || read.out.contains("[Pasted text") {
                landed = true;
                break;
            }
        }
        if !landed {
            return Some((false, "prompt never landed in pane".to_string()));
        }
        run30(&["herdr", "pane", "send-keys", pane, "Enter"]).await;
        run30(&["herdr", "pane", "send-keys", pane, "Enter"]).await;
        let timeout_ms = job.timeout.as_millis().to_string();
        let done = run(
            &[
                "herdr",
                "wait",
                "agent-status",
                pane,
                "--status",
                "done",
                "--timeout",
                &timeout_ms,
            ],
            job.timeout + Duration::from_secs(60),
        )
        .await;
        let tail = run30(&[
            "herdr", "pane", "read", pane, "--source", "recent", "--lines", "15",
        ])
        .await;
        println!(
            "[job:{}] agent {}; tail:\n{}",
            job.name,
            if done.ok { "done" } else { "TIMED OUT" },
            tail.out.trim()
        );
        Some((
            done.ok,
            if done.ok {
                "agent done".to_string()
            } else {
                "agent timeout".to_string()
            },
        ))
    }
    .await;
    // PLANT_KEEP_PANES=1: manual-verification override, leave the workspace open
    let close = std::env::var("PLANT_KEEP_PANES").as_deref() != Ok("1")
        && match job.close_pane {
            ClosePane::Always => true,
            ClosePane::OnSuccess => matches!(result, Some((true, _))),
        };
    if close {
        if let Some(ref id) = ws_id {
            close_workspace(id).await;
        }
    } else if ws_id.is_some() {
        println!(
            "[job:{}] pane kept open (close-pane: {:?})",
            job.name, job.close_pane
        );
    }
    result
}

pub async fn run_job(job: &Job) {
    let started = SystemTime::now();
    if job.kind == Kind::Compress {
        let ok =
            crate::sweep::compress_sweep(&crate::vault_path(), Duration::from_secs(3600)).await;
        let outcome = if ok { "success" } else { "failed" };
        record(&job.name, outcome, started, "compress sweep");
        return;
    }
    let Some(prompt) = prompt(job.kind) else {
        record(&job.name, "skipped", started, "nothing eligible");
        return;
    };
    match run_agent(job, &prompt).await {
        None => println!("[job:{}] herdr down, retry next tick", job.name),
        Some((true, detail)) => record(&job.name, "success", started, &detail),
        Some((false, detail)) => record(&job.name, "failed", started, &detail),
    }
}

pub async fn scheduler(cfg: Cfg) {
    if cfg.get("PLANT_JOBS").as_deref() == Some("0") {
        println!("[jobs] disabled (PLANT_JOBS=0)");
        return;
    }
    let cap: usize = cfg
        .get("PLANT_JOBS_MAX_CONCURRENT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let jobs = load_jobs();
    println!(
        "[jobs] scheduler: {} job(s) [{}], cap {cap}",
        jobs.len(),
        jobs.iter()
            .map(|j| j.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let sem = Arc::new(tokio::sync::Semaphore::new(cap));
    let running: Arc<Mutex<HashSet<String>>> = Default::default();
    tokio::time::sleep(Duration::from_secs(15)).await; // startup settle
    loop {
        for job in load_jobs() {
            if running.lock().unwrap().contains(&job.name) {
                continue;
            }
            let due = last_record_ts(&job.name)
                .is_none_or(|ts| epoch_now().saturating_sub(ts) >= job.every.as_secs());
            if !due {
                continue;
            }
            running.lock().unwrap().insert(job.name.clone());
            let (sem, running) = (sem.clone(), running.clone());
            tokio::spawn(async move {
                let _permit = sem.acquire().await;
                run_job(&job).await;
                running.lock().unwrap().remove(&job.name);
            });
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name(name: &str) -> Job {
        load_jobs().into_iter().find(|j| j.name == name).unwrap()
    }

    #[test]
    fn codex_learn_job_uses_requested_model_and_effort() {
        let job = by_name("learn-codex");
        assert_eq!(job.cli.as_deref(), Some("codex"));
        assert_eq!(job.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(job.args.as_deref(), Some("-c model_reasoning_effort=xhigh"));
        assert_eq!(job.close_pane, ClosePane::OnSuccess);
    }

    #[test]
    fn built_in_jobs_have_expected_shape() {
        let jobs = load_jobs();
        assert_eq!(jobs.len(), 4);
        assert_eq!(by_name("compress").kind, Kind::Compress);
        assert_eq!(by_name("learn").close_pane, ClosePane::OnSuccess);
        assert_eq!(by_name("reconcile").close_pane, ClosePane::Always);
        assert_eq!(by_name("reconcile").every, Duration::from_secs(30 * 86400));
    }

    #[test]
    fn launch_line_bypasses_shell_aliases() {
        assert!(launch_line(&by_name("learn-codex")).starts_with("command codex "));
        assert!(launch_line(&by_name("learn")).starts_with("command claude "));
    }

    #[test]
    fn reconcile_prompt_is_static_and_compress_has_none() {
        assert_eq!(prompt(Kind::Reconcile).as_deref(), Some("/Vault reconcile"));
        assert_eq!(prompt(Kind::Compress), None);
    }
}
