//! Built-in jobs, defined as Rust code in load_jobs()/run_job() below. Compress calls
//! the sweep directly in-process; learn/learn-codex/reconcile compute a prompt in Rust
//! and type it into a fresh Herdr pane.
//! The cleanup policy controls whether the job's Herdr workspace is
//! closed when the run ends; kept workspaces are reclaimed by the next run of the same job.
//! Outcomes append to ~/.local/state/plant/jobs/<name>.jsonl; the tail line is the
//! scheduling state (due when now - last.ts >= every). Never `claude -p`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::herdr::{self, AgentRun, AgentRunOutcome, WorkspaceCleanup};

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
pub enum Kind {
    Compress,
    Learn,
    LearnCodex,
    Reconcile,
    Validate,
    ValidateRepair,
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
    pub cleanup: WorkspaceCleanup,
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
            cleanup: WorkspaceCleanup::Always,
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
            cleanup: WorkspaceCleanup::OnSuccess,
            ..Job::new("learn", Kind::Learn, Duration::from_secs(15 * MIN))
        },
        Job {
            cli: Some("codex".into()),
            model: Some("gpt-5.6-sol".into()),
            args: Some("-c model_reasoning_effort=xhigh".into()),
            cleanup: WorkspaceCleanup::OnSuccess,
            ..Job::new(
                "learn-codex",
                Kind::LearnCodex,
                Duration::from_secs(15 * MIN),
            )
        },
        Job {
            cli: Some("codex".into()),
            model: Some("gpt-5.6-sol".into()),
            args: Some("-c model_reasoning_effort=xhigh".into()),
            ..Job::new("reconcile", Kind::Reconcile, Duration::from_secs(3600))
        },
        Job::new("validate", Kind::Validate, Duration::from_secs(3600)),
        Job {
            cli: Some("codex".into()),
            model: Some("gpt-5.6-sol".into()),
            // Stop hook injected per-invocation: codex may not stop until the vault
            // validates clean (the hook script self-limits to 5 continuations)
            args: Some(format!(
                "-c model_reasoning_effort=xhigh --dangerously-bypass-hook-trust \
                 -c 'hooks.Stop=[{{hooks=[{{type=\"command\",command=\"{}\",timeout=120}}]}}]'",
                stop_hook_path().display()
            )),
            cleanup: WorkspaceCleanup::OnSuccess,
            ..Job::new(
                "validate-repair",
                Kind::ValidateRepair,
                Duration::from_secs(3600),
            )
        },
    ]
}

fn stop_hook_path() -> PathBuf {
    state_dir().join("hooks/stop_until_valid.sh")
}

/// Idempotently (re)write the codex Stop hook script and reset its continuation counter.
fn write_stop_hook() -> std::io::Result<()> {
    let path = stop_hook_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let script = r#"#!/bin/sh
# plant validate-repair Stop hook: block codex from stopping until the vault validates.
cf="$HOME/.local/state/plant/hooks/validate-repair.count"
if vaultr validate >/dev/null 2>&1; then rm -f "$cf"; echo '{}'; exit 0; fi
n=$(( $(cat "$cf" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$cf"
if [ "$n" -ge 5 ]; then echo '{}'; exit 0; fi
echo '{"decision":"block","reason":"The vault is still invalid. Run `vaultr validate --json`, fix every remaining error (repair or remove broken [[wikilinks]], fix markdown path links, repair corrupt ledger lines, and for a preference-pool error consolidate vault/preferences/*.md under the byte cap by merging overlapping / shortening verbose / deleting stale preferences — never silently drop one). For intentional literal examples append <!-- vault-validate: ignore --> to that line instead of deleting it. Never touch vault/sessions/ capture data. Stop only when `vaultr validate` exits 0."}'
"#;
    std::fs::write(&path, script)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    let _ = std::fs::remove_file(state_dir().join("hooks/validate-repair.count"));
    Ok(())
}

/// Eligible session dirs for a learner, or None when there's nothing to learn from.
/// Claims the batch as in-flight (lease expiring `timeout` + slack from now) before
/// returning, so the next tick can't re-dispatch the same not-yet-ledgered sessions.
fn eligible(learner: &str, timeout: Duration) -> Option<String> {
    let vault = crate::vault_path();
    let list = crate::sweep::eligible_sessions(&vault, Duration::from_secs(3600), 10, learner);
    let (total, ledgered) = crate::sweep::eligibility_stats(&vault, learner);
    println!(
        "[eligible:{learner}] {} of {total} sessions ({ledgered} ledgered)",
        list.len()
    );
    if list.is_empty() {
        return None;
    }
    let sids: Vec<String> = list
        .iter()
        .filter_map(|d| {
            std::path::Path::new(d)
                .file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .collect();
    let expires_at = epoch_now() + timeout.as_secs() + 300;
    crate::sweep::claim_inflight(&vault, learner, &sids, expires_at);
    Some(list.join(" "))
}

/// The prompt an agent job types into its Herdr pane; None => nothing to do, skip.
fn prompt(job: &Job) -> Option<String> {
    match job.kind {
        Kind::Compress => None,
        Kind::Learn => eligible("claude", job.timeout)
            .map(|dirs| format!("/Vault learn --learner claude {dirs}")),
        Kind::LearnCodex => eligible("codex", job.timeout).map(|dirs| {
            format!(
                "$Vault Learn with `--learner codex` and these session directories as input: {dirs}"
            )
        }),
        Kind::Reconcile => Some("$Vault reconcile".to_string()),
        Kind::Validate => None,
        Kind::ValidateRepair => {
            let report = vault_validate()?;
            if report.errors() == 0 {
                return None;
            }
            let errors: Vec<String> = report
                .findings
                .iter()
                .filter(|f| f.severity == vaultr::validate::Severity::Error)
                .take(50)
                .map(|f| format!("- {} {}:{} {}", f.kind, f.file, f.line, f.detail))
                .collect();
            Some(format!(
                "The knowledge vault at ~/.dotfiles/vault has {} validation error(s). \
                 Fix them all: repair or remove broken [[wikilinks]] (prefer repairing the \
                 target), fix markdown path links, repair corrupt learnings/.ledger.jsonl \
                 lines; for a preference-pool error consolidate vault/preferences/*.md \
                 under the byte cap (merge overlapping, shorten verbose, delete stale — \
                 never silently drop a preference). For intentional literal examples append \
                 <!-- vault-validate: ignore --> to that line. Never touch vault/sessions/ \
                 capture data. Re-run `vaultr validate --json` and iterate until it exits 0.\n\
                 Current errors:\n{}",
                report.errors(),
                errors.join("\n")
            ))
        }
    }
}

/// Run the vault content validator; None on scan failure (skip this tick).
fn vault_validate() -> Option<vaultr::validate::Report> {
    let root = vaultr::validate::content_root(&crate::vault_path()).ok()?;
    vaultr::validate::scan(&root).ok()
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

fn cleanup_policy(job: &Job) -> WorkspaceCleanup {
    if std::env::var("PLANT_KEEP_PANES").as_deref() == Ok("0") {
        job.cleanup
    } else {
        WorkspaceCleanup::Never
    }
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
    if job.kind == Kind::Validate {
        match vault_validate() {
            Some(report) => {
                let outcome = if report.errors() == 0 { "success" } else { "failed" };
                record(&job.name, outcome, started, &report.summary());
            }
            None => record(&job.name, "failed", started, "vault scan failed"),
        }
        return;
    }
    let Some(prompt) = prompt(job) else {
        record(&job.name, "skipped", started, "nothing eligible");
        return;
    };
    if job.kind == Kind::ValidateRepair {
        if let Err(e) = write_stop_hook() {
            record(&job.name, "failed", started, &format!("stop hook write: {e}"));
            return;
        }
    }
    let agent_run = AgentRun {
        label: format!("job-{}", job.name),
        cwd: job.cwd.clone(),
        launch: launch_line(job),
        prompt,
        timeout: job.timeout,
        cleanup: cleanup_policy(job),
    };
    match herdr::run_agent(agent_run).await {
        AgentRunOutcome::Unavailable => {
            println!("[job:{}] herdr down, retry next tick", job.name)
        }
        AgentRunOutcome::Succeeded(detail) => record(&job.name, "success", started, &detail),
        AgentRunOutcome::Failed(detail) => record(&job.name, "failed", started, &detail),
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
        assert_eq!(job.cleanup, WorkspaceCleanup::OnSuccess);
    }

    #[test]
    fn built_in_jobs_have_expected_shape() {
        let jobs = load_jobs();
        assert_eq!(jobs.len(), 6);
        assert_eq!(by_name("validate").kind, Kind::Validate);
        let repair = by_name("validate-repair");
        assert_eq!(repair.cli.as_deref(), Some("codex"));
        let args = repair.args.unwrap();
        assert!(args.contains("--dangerously-bypass-hook-trust"));
        assert!(args.contains("hooks.Stop="));
        assert!(args.contains("stop_until_valid.sh"));
        assert_eq!(by_name("compress").kind, Kind::Compress);
        assert_eq!(by_name("learn").cleanup, WorkspaceCleanup::OnSuccess);
        assert_eq!(by_name("reconcile").cleanup, WorkspaceCleanup::Always);
        assert_eq!(by_name("reconcile").every, Duration::from_secs(3600));
    }

    #[test]
    fn launch_line_bypasses_shell_aliases() {
        assert!(launch_line(&by_name("learn-codex")).starts_with("command codex "));
        assert!(launch_line(&by_name("learn")).starts_with("command claude "));
    }

    #[test]
    fn reconcile_prompt_is_static_and_compress_has_none() {
        let reconcile = Job::new("reconcile", Kind::Reconcile, Duration::from_secs(3600));
        let compress = Job::new("compress", Kind::Compress, Duration::from_secs(3600));
        assert_eq!(prompt(&reconcile).as_deref(), Some("$Vault reconcile"));
        assert_eq!(prompt(&compress), None);
    }
}
