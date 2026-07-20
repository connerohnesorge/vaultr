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
//! renames take effect without a restart.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::herdr::WorkspaceCleanup;
use crate::state::{atomic_write, dir as state_dir, ensure_dir_durable, sync_dir};

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

fn ledger_path(name: &str) -> PathBuf {
    state_dir().join("jobs").join(format!("{name}.jsonl"))
}

fn last_record_ts(name: &str) -> io::Result<Option<u64>> {
    let text = match std::fs::read_to_string(ledger_path(name)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let record: serde_json::Value =
        serde_json::from_str(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    record
        .get("ts")
        .and_then(serde_json::Value::as_u64)
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "job record has no ts"))
}

#[derive(serde::Deserialize, serde::Serialize)]
struct AttemptFence {
    id: String,
    started_ts: u64,
    retryable: bool,
}

struct AttemptGuard {
    name: String,
    fence: AttemptFence,
    _lock: File,
}

enum AttemptStart {
    Ready(AttemptGuard),
    Blocked(String),
}

fn attempt_dir() -> PathBuf {
    state_dir().join("job-attempts")
}

fn attempt_path(name: &str) -> PathBuf {
    attempt_dir().join(format!("{name}.json"))
}

fn write_fence(path: &Path, fence: &AttemptFence) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(fence).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

fn ledger_has_attempt(name: &str, attempt_id: &str) -> io::Result<bool> {
    let text = match std::fs::read_to_string(ledger_path(name)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if record.get("attempt_id").and_then(serde_json::Value::as_str) == Some(attempt_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn verify_ledger_writable(name: &str) -> io::Result<()> {
    let dir = state_dir().join("jobs");
    ensure_dir_durable(&dir)?;
    let path = ledger_path(name);
    if path.exists() {
        OpenOptions::new().append(true).open(&path)?;
        last_record_ts(name)?;
        return Ok(());
    }
    let marker = dir.join(format!(".{name}.probe-{}", uuid::Uuid::new_v4()));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .and_then(|file| file.sync_all());
    let _ = std::fs::remove_file(marker);
    result
}

fn begin_attempt(name: &str) -> io::Result<AttemptStart> {
    let dir = attempt_dir();
    ensure_dir_durable(&dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{name}.lock")))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::WouldBlock {
            Ok(AttemptStart::Blocked(
                "another process holds the attempt lock".to_string(),
            ))
        } else {
            Err(error)
        };
    }
    verify_ledger_writable(name)?;

    let path = attempt_path(name);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(
            serde_json::from_str::<AttemptFence>(&text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(existing) = existing {
        if !existing.retryable && !ledger_has_attempt(name, &existing.id)? {
            return Ok(AttemptStart::Blocked(format!(
                "attempt {} has no durable final outcome",
                existing.id
            )));
        }
        std::fs::remove_file(&path)?;
        sync_dir(&dir)?;
    }

    let fence = AttemptFence {
        id: uuid::Uuid::new_v4().to_string(),
        started_ts: epoch_now(),
        retryable: false,
    };
    write_fence(&path, &fence)?;
    Ok(AttemptStart::Ready(AttemptGuard {
        name: name.to_string(),
        fence,
        _lock: lock,
    }))
}

impl AttemptGuard {
    fn mark_retryable(&mut self) -> io::Result<()> {
        self.fence.retryable = true;
        write_fence(&attempt_path(&self.name), &self.fence)
    }

    fn clear(&self) -> io::Result<()> {
        std::fs::remove_file(attempt_path(&self.name))?;
        sync_dir(&attempt_dir())
    }
}

fn record(
    name: &str,
    attempt_id: &str,
    outcome: &str,
    started: SystemTime,
    detail: &str,
) -> io::Result<()> {
    let dir = state_dir().join("jobs");
    ensure_dir_durable(&dir)?;
    let rec = serde_json::json!({
        "ts": epoch_now(),
        "iso": crate::capture::iso_now(),
        "attempt_id": attempt_id,
        "outcome": outcome,
        "duration_ms": started.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0),
        "detail": detail,
    });
    let mut line =
        serde_json::to_vec(&rec).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path(name))?;
    file.write_all(&line)?;
    file.sync_all()?;
    sync_dir(&dir)?;
    println!("[job:{name}] {outcome} ({detail})");
    Ok(())
}

fn finish_attempt(
    attempt: &AttemptGuard,
    outcome: &str,
    started: SystemTime,
    detail: &str,
) -> io::Result<()> {
    record(&attempt.name, &attempt.fence.id, outcome, started, detail)?;
    attempt.clear()
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

enum JobExecution {
    Final {
        code: i32,
        outcome: &'static str,
        detail: String,
    },
    Retryable {
        detail: String,
    },
}

async fn execute_job(job: &Job) -> JobExecution {
    // Exec the script directly: the shebang picks the interpreter. A missing
    // shebang or exec bit fails at spawn (ENOEXEC/EACCES).
    let mut cmd = tokio::process::Command::new(&job.path);
    cmd.current_dir(expand_home("~/.dotfiles"))
        .env("PATH", script_path_env())
        .env(
            "PLANT_LAST_TS",
            last_record_ts(&job.name)
                .ok()
                .flatten()
                .unwrap_or(0)
                .to_string(),
        )
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            return JobExecution::Final {
                code: 1,
                outcome: "failed",
                detail: format!("spawn: {error}"),
            };
        }
    };
    let out = match tokio::time::timeout(SCRIPT_BACKSTOP, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(error)) => {
            return JobExecution::Final {
                code: 1,
                outcome: "failed",
                detail: format!("wait: {error}"),
            };
        }
        Err(_) => {
            // kill_on_drop reaped the child when the future was dropped by timeout
            return JobExecution::Final {
                code: 1,
                outcome: "failed",
                detail: "killed: 3h backstop".to_string(),
            };
        }
    };
    let detail = tail_line(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| tail_line(&String::from_utf8_lossy(&out.stderr)))
        .unwrap_or_else(|| "no output".to_string());
    let code = out.status.code();
    match outcome_for(code) {
        Some(outcome) => JobExecution::Final {
            code: if code == Some(0) { 0 } else { 1 },
            outcome,
            detail,
        },
        None => JobExecution::Retryable { detail },
    }
}

pub async fn run_job(job: &Job) -> i32 {
    let started = SystemTime::now();
    let mut attempt = match begin_attempt(&job.name) {
        Ok(AttemptStart::Ready(attempt)) => attempt,
        Ok(AttemptStart::Blocked(detail)) => {
            eprintln!("[job:{}] dispatch blocked: {detail}", job.name);
            return 1;
        }
        Err(e) => {
            eprintln!("[job:{}] attempt fence failed: {e}", job.name);
            return 1;
        }
    };

    match execute_job(job).await {
        JobExecution::Final {
            code,
            outcome,
            detail,
        } => {
            if let Err(error) = finish_attempt(&attempt, outcome, started, &detail) {
                eprintln!("[job:{}] final record failed: {error}", job.name);
                return 1;
            }
            code
        }
        JobExecution::Retryable { detail } => {
            if let Err(e) = attempt.mark_retryable() {
                eprintln!("[job:{}] retry fence failed: {e}", job.name);
                return 1;
            }
            println!("[job:{}] retry next tick ({detail})", job.name);
            75
        }
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
            let due = match last_record_ts(&job.name) {
                Ok(last) => {
                    last.is_none_or(|ts| epoch_now().saturating_sub(ts) >= job.every.as_secs())
                }
                Err(e) => {
                    eprintln!("[job:{}] ledger unreadable: {e}", job.name);
                    false
                }
            };
            if !due {
                continue;
            }
            running.lock().unwrap().insert(job.name.clone());
            let (sem, running) = (sem.clone(), running.clone());
            tokio::spawn(async move {
                let _permit = sem.acquire().await;
                let _ = run_job(&job).await;
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
