//! SwiftBar-style job scheduler: jobs are executable scripts at
//! <vault content root>/jobs/<name>.<interval>.<ext> (e.g. learn.15m.sh,
//! door-oncall.30m.ts). The filename carries the cadence; the script is exec'd
//! directly, so its shebang picks the interpreter (bash, bun, …) — Plant maps no
//! extensions. The script body does the work itself, composing the plant/vaultr
//! CLIs (`plant sessions eligible --claim`, `plant agent run`, `vaultr validate`, …).
//! Agent-backed jobs MUST go through `plant agent run` (Herdr pane orchestration) —
//! never `claude -p`.
//! Outcomes append to ~/.local/state/plant/jobs/<name>.jsonl; the tail line is the
//! scheduling state (due when now - last.ts >= every). Exit code contract:
//! 0 = success, 75 = retry next tick without recording (EX_TEMPFAIL, e.g. herdr down),
//! anything else = failed. The job set is rescanned every tick — edits and interval
//! renames take effect without a restart. The `compress` cadence marker runs
//! its sweep directly in the listener-owning daemon; every other job executes
//! its script.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::herdr::WorkspaceCleanup;

pub fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state/plant")
}

/// Config keys read from real env first, then the vault/.env fallback. The fallback
/// covers exactly the keys the job scheduler consults through Cfg: PLANT_JOBS
/// (0 disables the scheduler), PLANT_JOBS_MAX_CONCURRENT (semaphore cap), and
/// PLANT_KEEP_PANES (0 opts into per-job workspace cleanup; see cleanup_policy).
/// Other PLANT_* keys (e.g. herdr.rs's PLANT_HERDR_INTERVAL_SECS, snapshot path)
/// remain real-env only. A private map, never std::env::set_var: nothing here may
/// leak into child process environments.
pub struct Cfg(HashMap<String, String>);

impl Cfg {
    pub fn load(vault_sessions: &std::path::Path) -> Self {
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

    #[cfg(test)]
    fn from_map(map: HashMap<String, String>) -> Self {
        Cfg(map)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub name: String,
    pub path: PathBuf,
    pub every: Duration,
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

/// "learn.15m.sh" -> ("learn", 900s). Extension-agnostic: anything matching
/// <name>.<interval>.<ext>. None otherwise (those files are skipped by the scanner).
pub fn parse_job_filename(file_name: &str) -> Option<(String, Duration)> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    let (name, interval) = stem.rsplit_once('.')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), parse_duration(interval)?))
}

pub fn jobs_dir() -> Option<PathBuf> {
    vaultr::validate::content_root(&crate::vault_root())
        .ok()
        .map(|root| root.join("jobs"))
}

pub fn load_jobs() -> Vec<Job> {
    let Some(dir) = jobs_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut jobs: Vec<Job> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let file_name = path.file_name()?.to_str()?.to_string();
            let (name, every) = parse_job_filename(&file_name)?;
            Some(Job { name, path, every })
        })
        .collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    jobs
}

/// Panes are kept open by default so failed agent runs stay inspectable; setting
/// PLANT_KEEP_PANES=0 opts into auto-close honoring the requested cleanup.
/// Keep-by-default is a recorded product decision (commit 3f9d55e) — do not invert it.
pub fn cleanup_policy(requested: WorkspaceCleanup, cfg: &Cfg) -> WorkspaceCleanup {
    if cfg.get("PLANT_KEEP_PANES").as_deref() == Some("0") {
        requested
    } else {
        WorkspaceCleanup::Never
    }
}

/// Launch line for an agent CLI inside a Herdr pane.
/// `command` bypasses the user's interactive-shell aliases (a `codex='codex --yolo'`
/// alias duplicated our flag, clap refused, and the prompt got typed into bare zsh).
pub fn launch_line(cli: &str, model: Option<&str>, args: Option<&str>) -> String {
    let codex = cli == "codex";
    let mut s = if codex {
        // sandboxed codex blocks on its first approval prompt — background panes can't answer
        "command codex --dangerously-bypass-approvals-and-sandbox".to_string()
    } else {
        "command claude --dangerously-skip-permissions".to_string()
    };
    if let Some(m) = model {
        s.push_str(&if codex {
            format!(" -m '{m}'")
        } else {
            format!(" --model='{m}'")
        });
    }
    if let Some(a) = args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

pub fn epoch_now() -> u64 {
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

/// Exit status -> ledger outcome. None = don't record, retry next tick (75/EX_TEMPFAIL
/// mirrors the old herdr-Unavailable behavior; a killed process has no code => failed).
fn outcome_for(code: Option<i32>) -> Option<&'static str> {
    match code {
        Some(0) => Some("success"),
        Some(75) => None,
        _ => Some("failed"),
    }
}

/// PATH for job scripts: launchd's env is minimal, so prepend the running binary's
/// own dir (plant) and the Home Manager profile bin (vaultr, jq, bash deps).
fn script_path_env() -> String {
    let mut parts = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.display().to_string()))
    {
        parts.push(dir);
    }
    parts.push(expand_home("~/.nix-profile/bin"));
    parts.push("/usr/bin:/bin".to_string());
    if let Ok(p) = std::env::var("PATH") {
        parts.push(p);
    }
    parts.join(":")
}

fn tail_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
}

/// Runaway backstop only — scripts own their real timeouts (passed to `plant agent run`).
const SCRIPT_BACKSTOP: Duration = Duration::from_secs(3 * 3600);

pub async fn run_job(job: &Job) -> i32 {
    let started = SystemTime::now();
    // Exec the script directly: the shebang picks the interpreter. A missing
    // shebang or exec bit fails at spawn (ENOEXEC/EACCES) and is recorded below.
    let mut cmd = tokio::process::Command::new(&job.path);
    cmd.current_dir(expand_home("~/.dotfiles"))
        .env("PATH", script_path_env())
        .env(
            "PLANT_LAST_TS",
            last_record_ts(&job.name).unwrap_or(0).to_string(),
        )
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            record(&job.name, "failed", started, &format!("spawn: {e}"));
            return 1;
        }
    };
    let out = match tokio::time::timeout(SCRIPT_BACKSTOP, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            record(&job.name, "failed", started, &format!("wait: {e}"));
            return 1;
        }
        Err(_) => {
            // kill_on_drop reaped the child when the future was dropped by timeout
            record(&job.name, "failed", started, "killed: 3h backstop");
            return 1;
        }
    };
    let detail = tail_line(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| tail_line(&String::from_utf8_lossy(&out.stderr)))
        .unwrap_or_else(|| "no output".to_string());
    let code = out.status.code();
    match outcome_for(code) {
        Some(outcome) => record(&job.name, outcome, started, &detail),
        None => println!("[job:{}] retry next tick ({detail})", job.name),
    }
    match code {
        Some(0) => 0,
        Some(75) => 75,
        _ => 1,
    }
}

async fn run_compress_job(job: &Job, vault: &std::path::Path) -> i32 {
    let started = SystemTime::now();
    match crate::sweep::compress_sweep(vault, Duration::from_secs(3600)).await {
        Ok(()) => {
            record(&job.name, "success", started, "in-process sweep complete");
            0
        }
        Err(error) => {
            record(&job.name, "failed", started, &error);
            1
        }
    }
}

pub async fn scheduler(cfg: Cfg, vault: PathBuf) {
    if cfg.get("PLANT_JOBS").as_deref() == Some("0") {
        println!("[jobs] disabled (PLANT_JOBS=0)");
        return;
    }
    let cap: usize = cfg
        .get("PLANT_JOBS_MAX_CONCURRENT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let sem = Arc::new(tokio::sync::Semaphore::new(cap));
    let running: Arc<Mutex<HashSet<String>>> = Default::default();
    let mut last_seen: Option<Vec<String>> = None;
    tokio::time::sleep(Duration::from_secs(15)).await; // startup settle
    loop {
        let jobs = load_jobs();
        let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
        if last_seen.as_ref() != Some(&names) {
            match jobs_dir() {
                Some(dir) if jobs.is_empty() => {
                    println!(
                        "[jobs] NO job scripts at {} — nothing scheduled",
                        dir.display()
                    )
                }
                _ => println!(
                    "[jobs] {} job(s) [{}], cap {cap}",
                    jobs.len(),
                    names.join(", ")
                ),
            }
            last_seen = Some(names);
        }
        for job in jobs {
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
            let vault = vault.clone();
            tokio::spawn(async move {
                let _permit = sem.acquire().await;
                if job.name == "compress" {
                    run_compress_job(&job, &vault).await;
                } else {
                    run_job(&job).await;
                }
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
    fn job_filenames_parse_name_and_interval() {
        assert_eq!(
            parse_job_filename("learn.15m.sh"),
            Some(("learn".to_string(), Duration::from_secs(15 * 60)))
        );
        assert_eq!(
            parse_job_filename("learn-codex.15m.sh"),
            Some(("learn-codex".to_string(), Duration::from_secs(15 * 60)))
        );
        assert_eq!(
            parse_job_filename("compress.30m.sh"),
            Some(("compress".to_string(), Duration::from_secs(30 * 60)))
        );
        assert_eq!(
            parse_job_filename("watchdog.6h.sh"),
            Some(("watchdog".to_string(), Duration::from_secs(6 * 3600)))
        );
        // dotted names keep everything before the final interval segment
        assert_eq!(
            parse_job_filename("a.b.2h.sh"),
            Some(("a.b".to_string(), Duration::from_secs(2 * 3600)))
        );
        // extension-agnostic: shebang picks the interpreter, not Plant
        assert_eq!(
            parse_job_filename("door-oncall.30m.ts"),
            Some(("door-oncall".to_string(), Duration::from_secs(30 * 60)))
        );
        for bad in [
            "README.md",
            "noninterval.sh",
            "bad.xx.sh",
            ".15m.sh",
            "learn.15m",
        ] {
            assert_eq!(parse_job_filename(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn exit_codes_map_to_ledger_outcomes() {
        assert_eq!(outcome_for(Some(0)), Some("success"));
        assert_eq!(outcome_for(Some(75)), None, "EX_TEMPFAIL retries silently");
        assert_eq!(outcome_for(Some(1)), Some("failed"));
        assert_eq!(outcome_for(Some(2)), Some("failed"));
        assert_eq!(outcome_for(None), Some("failed"), "signal-killed = failed");
    }

    #[test]
    fn launch_line_bypasses_shell_aliases() {
        assert_eq!(
            launch_line(
                "codex",
                Some("gpt-5.6-sol"),
                Some("-c model_reasoning_effort=xhigh")
            ),
            "command codex --dangerously-bypass-approvals-and-sandbox -m 'gpt-5.6-sol' \
             -c model_reasoning_effort=xhigh"
        );
        assert_eq!(
            launch_line("claude", Some("opus[1m]"), None),
            "command claude --dangerously-skip-permissions --model='opus[1m]'"
        );
    }

    #[test]
    fn cleanup_policy_keeps_panes_unless_cfg_opts_out() {
        // Guard: real-env PLANT_KEEP_PANES would shadow the map (Cfg checks env first).
        assert!(std::env::var("PLANT_KEEP_PANES").is_err());
        // Default (key absent anywhere): panes kept, per commit 3f9d55e.
        let keep = Cfg::from_map(HashMap::new());
        assert_eq!(
            cleanup_policy(WorkspaceCleanup::OnSuccess, &keep),
            WorkspaceCleanup::Never
        );
        // vault/.env fallback opts into the requested policy, same as the real env.
        let opt_out = Cfg::from_map(HashMap::from([(
            "PLANT_KEEP_PANES".to_string(),
            "0".to_string(),
        )]));
        assert_eq!(
            cleanup_policy(WorkspaceCleanup::OnSuccess, &opt_out),
            WorkspaceCleanup::OnSuccess
        );
    }

    #[test]
    fn tail_line_picks_last_nonempty_and_truncates() {
        assert_eq!(tail_line("a\nb\n\n  \n"), Some("b".to_string()));
        assert_eq!(tail_line(""), None);
        let long = format!("first\n{}", "x".repeat(400));
        assert_eq!(tail_line(&long).unwrap().len(), 300);
    }
}
