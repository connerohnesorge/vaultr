//! Declarative jobs: <vault>/jobs/*.md — frontmatter (schedule + launch config)
//! + body. `cli:` present => agent job (render !`cmd` placeholders, type into a fresh
//! Herdr pane); absent => script job (execute the body's !`cmd` lines in order).
//! Outcomes append to ~/.local/state/plant/jobs/<name>.jsonl; the tail line is the
//! scheduling state (due when now - last.ts >= every). Never `claude -p`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sweep::{run, run30};

const DEFAULTS: &[(&str, &str)] = &[
    ("learn", include_str!("../jobs/learn.md")),
    ("learn-codex", include_str!("../jobs/learn-codex.md")),
    ("compress", include_str!("../jobs/compress.md")),
    ("reconcile", include_str!("../jobs/reconcile.md")),
];

pub fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state/plant")
}

/// Jobs live in the user's vault: sibling of the sessions dir (VAULT_SESSIONS or
/// ~/.dotfiles/vault/sessions) => <vault>/jobs.
fn jobs_dir() -> PathBuf {
    let sessions = std::env::var("VAULT_SESSIONS").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".dotfiles/vault/sessions")
    });
    sessions.parent().map(Path::to_path_buf).unwrap_or(sessions).join("jobs")
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

#[derive(Clone)]
pub struct Job {
    pub name: String,
    pub every: Duration,
    pub enabled: bool,
    pub timeout: Duration,
    pub cli: Option<String>,
    pub model: Option<String>,
    pub config: Option<String>,
    pub args: Option<String>,
    pub cwd: String,
    pub body: String,
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

pub fn parse_job(name: &str, text: &str) -> Option<Job> {
    let rest = text.strip_prefix("---")?;
    let (fm, body) = rest.split_once("\n---")?;
    let mut job = Job {
        name: name.to_string(),
        every: Duration::from_secs(15 * 60),
        enabled: true,
        timeout: Duration::from_secs(45 * 60),
        cli: None,
        model: None,
        config: None,
        args: None,
        cwd: expand_home("~/.dotfiles"),
        body: body.trim().to_string(),
    };
    for line in fm.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "every" => job.every = parse_duration(v)?,
            "enabled" => job.enabled = v != "false",
            "timeout" => job.timeout = parse_duration(v)?,
            "cli" => job.cli = Some(v.to_string()),
            "model" => job.model = Some(v.to_string()),
            "config" => job.config = Some(expand_home(v)),
            "args" => job.args = Some(v.to_string()),
            "cwd" => job.cwd = expand_home(v),
            _ => {}
        }
    }
    Some(job)
}

pub fn load_jobs() -> Vec<Job> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(jobs_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            match std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| parse_job(&name, &t))
            {
                Some(j) => out.push(j),
                None => eprintln!("[jobs] {name}.md: parse failed, skipping"),
            }
        }
    }
    if out.is_empty() {
        // jobs/ missing or empty: compiled-in defaults keep capture hygiene alive
        out.extend(DEFAULTS.iter().filter_map(|(n, t)| parse_job(n, t)));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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

/// sh -c with the job's cwd; augmented PATH so job bodies can say `plant sessions
/// eligible` (plant's own dir) and reach user-installed tools under launchd's bare env.
async fn shell(cmd: &str, cwd: &str, timeout: Duration) -> (bool, String) {
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .env("PATH", crate::sweep::augmented_path())
        .output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        _ => (false, String::new()),
    }
}

/// Substitute !`cmd` placeholders (line start or after whitespace, same rule as
/// Claude Code skills). Err(cmd) on the first non-zero exit => skip the run.
async fn render(body: &str, cwd: &str, timeout: Duration) -> Result<String, String> {
    let re = regex::Regex::new(r"!`([^`]+)`").expect("static regex");
    let mut out = String::new();
    for line in body.lines() {
        let spans: Vec<(usize, usize)> = re
            .find_iter(line)
            .map(|m| (m.start(), m.end()))
            .filter(|&(s, _)| s == 0 || line[..s].ends_with(char::is_whitespace))
            .collect();
        let mut rendered = String::new();
        let mut last = 0;
        for (s, e) in spans {
            rendered.push_str(&line[last..s]);
            let cmd = &line[s + 2..e - 1];
            let (ok, output) = shell(cmd, cwd, timeout).await;
            if !ok {
                return Err(cmd.to_string());
            }
            rendered.push_str(&output);
            last = e;
        }
        rendered.push_str(&line[last..]);
        out.push_str(&rendered);
        out.push('\n');
    }
    Ok(out.trim().to_string())
}

fn launch_line(job: &Job) -> String {
    let codex = job.cli.as_deref() == Some("codex");
    let mut s = if codex {
        // sandboxed codex blocks on its first approval prompt — background panes can't answer
        "codex --dangerously-bypass-approvals-and-sandbox".to_string()
    } else {
        "claude --dangerously-skip-permissions".to_string()
    };
    if let Some(m) = &job.model {
        s.push_str(&if codex {
            format!(" -m '{m}'")
        } else {
            format!(" --model='{m}'")
        });
    }
    if let Some(c) = &job.config {
        s.push_str(&if codex {
            format!(" --profile '{c}'")
        } else {
            format!(" --settings '{c}'")
        });
    }
    if let Some(a) = &job.args {
        s.push(' ');
        s.push_str(a);
    }
    s
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
            if read.out.contains(&needle) {
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
    if let Some(ref id) = ws_id {
        close_workspace(id).await;
    }
    result
}

async fn run_job(job: &Job) {
    let started = SystemTime::now();
    if job.cli.is_some() {
        let rendered = match render(&job.body, &job.cwd, job.timeout).await {
            Ok(r) if r.is_empty() => {
                record(&job.name, "skipped", started, "empty rendered body");
                return;
            }
            Ok(r) => r,
            Err(cmd) => {
                record(
                    &job.name,
                    "skipped",
                    started,
                    &format!("precondition: {cmd}"),
                );
                return;
            }
        };
        match run_agent(job, &rendered).await {
            None => println!("[job:{}] herdr down, retry next tick", job.name),
            Some((true, detail)) => record(&job.name, "success", started, &detail),
            Some((false, detail)) => record(&job.name, "failed", started, &detail),
        }
    } else {
        // script job: only !`cmd` lines execute; anything else is prose
        for line in job.body.lines() {
            let Some(cmd) = line
                .trim()
                .strip_prefix("!`")
                .and_then(|r| r.strip_suffix('`'))
            else {
                continue;
            };
            let (ok, out) = shell(cmd, &job.cwd, job.timeout).await;
            if !out.is_empty() {
                println!("[job:{}] {out}", job.name);
            }
            if !ok {
                record(
                    &job.name,
                    "failed",
                    started,
                    &format!("command failed: {cmd}"),
                );
                return;
            }
        }
        record(&job.name, "success", started, "script ok");
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
            if !job.enabled || running.lock().unwrap().contains(&job.name) {
                continue;
            }
            let due = last_record_ts(&job.name).map_or(true, |ts| {
                epoch_now().saturating_sub(ts) >= job.every.as_secs()
            });
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

    #[test]
    fn codex_learn_job_uses_requested_model_and_effort() {
        let job = parse_job("learn-codex", include_str!("../jobs/learn-codex.md")).unwrap();
        assert_eq!(job.cli.as_deref(), Some("codex"));
        assert_eq!(job.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(job.args.as_deref(), Some("-c model_reasoning_effort=xhigh"));
        assert!(job.body.contains("--learner codex"));
    }
}
