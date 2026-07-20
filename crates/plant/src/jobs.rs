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

#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::domain::Harness;
use crate::herdr::WorkspaceCleanup;
use crate::state::{atomic_write, dir as state_dir, ensure_dir_durable, sync_dir};

mod config;
#[cfg(test)]
mod scheduled_tests;
pub(crate) use config::Cfg;

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub name: String,
    pub path: PathBuf,
    pub every: Duration,
    pub action: JobAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobAction {
    Script,
    InProcessCompression,
}

fn action_for(name: &str) -> JobAction {
    if name == "compress" {
        JobAction::InProcessCompression
    } else {
        JobAction::Script
    }
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
            let action = action_for(&name);
            Some(Job {
                name,
                path,
                every,
                action,
            })
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
pub fn launch_line(harness: Harness, model: Option<&str>, args: Option<&str>) -> String {
    let mut s = match harness {
        Harness::ClaudeCode => "command claude --dangerously-skip-permissions".to_string(),
        // sandboxed codex blocks on its first approval prompt — background panes can't answer
        Harness::Codex => "command codex --dangerously-bypass-approvals-and-sandbox".to_string(),
    };
    if let Some(m) = model {
        match harness {
            Harness::ClaudeCode => s.push_str(&format!(" --model='{m}'")),
            Harness::Codex => s.push_str(&format!(" -m '{m}'")),
        }
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
    last_record_ts_at(&ledger_path(name))
}

fn last_record_ts_at(path: &Path) -> io::Result<Option<u64>> {
    let text = match std::fs::read_to_string(path) {
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

fn record_is_due(last: Option<u64>, every: Duration, now: u64) -> bool {
    last.is_none_or(|ts| now.saturating_sub(ts) >= every.as_secs())
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

fn ledger_has_attempt_at<F>(
    path: &Path,
    parent: &Path,
    attempt_id: &str,
    sync: F,
) -> io::Result<bool>
where
    F: FnOnce(&File) -> io::Result<()>,
{
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular ledger", path.display()),
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if record.get("attempt_id").and_then(serde_json::Value::as_str) == Some(attempt_id) {
            // Visibility in the page cache is not recovery evidence. The exact
            // descriptor whose matching record we parsed must be synchronizable,
            // and the containing directory must remain durable, before a fence
            // may be cleared.
            sync(&file)?;
            sync_dir(parent)?;
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

#[derive(Debug)]
enum FenceReconcile {
    Ready,
    Blocked(String),
}

fn reconcile_fence_at<F>(
    fence_path: &Path,
    attempt_parent: &Path,
    ledger_path: &Path,
    ledger_parent: &Path,
    sync: F,
) -> io::Result<FenceReconcile>
where
    F: FnOnce(&File) -> io::Result<()>,
{
    let existing = match std::fs::read_to_string(fence_path) {
        Ok(text) => Some(
            serde_json::from_str::<AttemptFence>(&text)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let Some(existing) = existing else {
        return Ok(FenceReconcile::Ready);
    };
    if !existing.retryable
        && !ledger_has_attempt_at(ledger_path, ledger_parent, &existing.id, sync)?
    {
        return Ok(FenceReconcile::Blocked(format!(
            "attempt {} has no durable final outcome",
            existing.id
        )));
    }
    std::fs::remove_file(fence_path)?;
    sync_dir(attempt_parent)?;
    Ok(FenceReconcile::Ready)
}

fn reconcile_fence(name: &str) -> io::Result<FenceReconcile> {
    let ledger = ledger_path(name);
    reconcile_fence_at(
        &attempt_path(name),
        &attempt_dir(),
        &ledger,
        ledger.parent().expect("job ledger has a parent"),
        File::sync_all,
    )
}

enum AttemptLockStart {
    Ready(File),
    Blocked(String),
}

fn acquire_attempt_lock(name: &str) -> io::Result<AttemptLockStart> {
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
            Ok(AttemptLockStart::Blocked(
                "another process holds the attempt lock".to_string(),
            ))
        } else {
            Err(error)
        };
    }
    Ok(AttemptLockStart::Ready(lock))
}

fn publish_attempt(name: &str, lock: File) -> io::Result<AttemptGuard> {
    let fence = AttemptFence {
        id: uuid::Uuid::new_v4().to_string(),
        started_ts: epoch_now(),
        retryable: false,
    };
    write_fence(&attempt_path(name), &fence)?;
    Ok(AttemptGuard {
        name: name.to_string(),
        fence,
        _lock: lock,
    })
}

fn begin_attempt_locked(name: &str, lock: File) -> io::Result<AttemptStart> {
    verify_ledger_writable(name)?;
    match reconcile_fence(name)? {
        FenceReconcile::Ready => publish_attempt(name, lock).map(AttemptStart::Ready),
        FenceReconcile::Blocked(detail) => Ok(AttemptStart::Blocked(detail)),
    }
}

fn begin_attempt(name: &str) -> io::Result<AttemptStart> {
    match acquire_attempt_lock(name)? {
        AttemptLockStart::Ready(lock) => begin_attempt_locked(name, lock),
        AttemptLockStart::Blocked(detail) => Ok(AttemptStart::Blocked(detail)),
    }
}

enum ScheduledAttemptStart {
    Ready(AttemptGuard),
    NotDue,
    Blocked(String),
}

fn begin_scheduled_attempt(job: &Job) -> io::Result<ScheduledAttemptStart> {
    let lock = match acquire_attempt_lock(&job.name)? {
        AttemptLockStart::Ready(lock) => lock,
        AttemptLockStart::Blocked(detail) => return Ok(ScheduledAttemptStart::Blocked(detail)),
    };
    verify_ledger_writable(&job.name)?;
    if let FenceReconcile::Blocked(detail) = reconcile_fence(&job.name)? {
        return Ok(ScheduledAttemptStart::Blocked(detail));
    }
    let due = record_is_due(last_record_ts(&job.name)?, job.every, epoch_now());
    if !due {
        return Ok(ScheduledAttemptStart::NotDue);
    }
    publish_attempt(&job.name, lock).map(ScheduledAttemptStart::Ready)
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

fn script_working_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_SCRIPT_CWD.with(|slot| slot.borrow().clone()) {
        return path;
    }
    PathBuf::from(expand_home("~/.dotfiles"))
}

#[cfg(test)]
thread_local! {
    static TEST_SCRIPT_CWD: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
struct TestScriptCwd;

#[cfg(test)]
impl Drop for TestScriptCwd {
    fn drop(&mut self) {
        TEST_SCRIPT_CWD.with(|slot| {
            slot.replace(None);
        });
    }
}

#[cfg(test)]
fn use_test_script_cwd(path: PathBuf) -> TestScriptCwd {
    TEST_SCRIPT_CWD.with(|slot| {
        assert!(
            slot.replace(Some(path)).is_none(),
            "test cwd already scoped"
        );
    });
    TestScriptCwd
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

#[derive(Debug, PartialEq)]
enum JobExecution {
    Succeeded(String),
    Failed(String),
    Retryable(String),
}

async fn execute_script(job: &Job) -> JobExecution {
    execute_script_with_timeout(job, SCRIPT_BACKSTOP).await
}

async fn execute_script_with_timeout(job: &Job, timeout: Duration) -> JobExecution {
    // Exec the script directly: the shebang picks the interpreter. A missing
    // shebang or exec bit fails at spawn (ENOEXEC/EACCES).
    let mut cmd = tokio::process::Command::new(&job.path);
    cmd.current_dir(script_working_dir())
        .env("PATH", script_path_env())
        .env(
            "PLANT_LAST_TS",
            last_record_ts(&job.name)
                .ok()
                .flatten()
                .unwrap_or(0)
                .to_string(),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let run = crate::process::run_command(&mut cmd, timeout).await;
    let detail = tail_line(&run.out)
        .or_else(|| tail_line(&run.stderr))
        .unwrap_or_else(|| "no output".to_string());
    match run.end {
        crate::process::RunEnd::Exited(Some(0)) => JobExecution::Succeeded(detail),
        crate::process::RunEnd::Exited(Some(75)) => JobExecution::Retryable(detail),
        crate::process::RunEnd::Exited(_) => JobExecution::Failed(detail),
        crate::process::RunEnd::TimedOut => JobExecution::Failed(format!(
            "killed and reaped: {}s backstop",
            timeout.as_secs()
        )),
        crate::process::RunEnd::SpawnFailed => {
            JobExecution::Failed(format!("spawn: {}", run.stderr))
        }
        crate::process::RunEnd::WaitFailed => {
            JobExecution::Failed(format!("wait/reap: {}", run.stderr))
        }
    }
}

async fn execute_compression(vault: &Path) -> JobExecution {
    if crate::sweep::compress_sweep(vault, Duration::from_secs(3600)).await {
        JobExecution::Succeeded("in-process compression complete".to_string())
    } else {
        JobExecution::Failed("in-process compression failed".to_string())
    }
}

async fn execute_scheduled(job: &Job, vault: &Path, script_timeout: Duration) -> JobExecution {
    match job.action {
        JobAction::Script => execute_script_with_timeout(job, script_timeout).await,
        JobAction::InProcessCompression => execute_compression(vault).await,
    }
}

fn finish_execution(
    job: &Job,
    mut attempt: AttemptGuard,
    started: SystemTime,
    execution: JobExecution,
) -> i32 {
    match execution {
        JobExecution::Succeeded(detail) => {
            if let Err(error) = finish_attempt(&attempt, "success", started, &detail) {
                eprintln!("[job:{}] final record failed: {error}", job.name);
                return 1;
            }
            0
        }
        JobExecution::Failed(detail) => {
            if let Err(error) = finish_attempt(&attempt, "failed", started, &detail) {
                eprintln!("[job:{}] final record failed: {error}", job.name);
            }
            1
        }
        JobExecution::Retryable(detail) => {
            if let Err(error) = attempt.mark_retryable() {
                eprintln!("[job:{}] retry fence failed: {error}", job.name);
                return 1;
            }
            println!("[job:{}] retry next tick ({detail})", job.name);
            75
        }
    }
}

pub async fn run_job(job: &Job) -> i32 {
    let attempt = match begin_attempt(&job.name) {
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
    // Manual runs deliberately execute the marker/wrapper script. Only daemon
    // discovery dispatches typed in-process actions.
    let started = SystemTime::now();
    let execution = execute_script(job).await;
    finish_execution(job, attempt, started, execution)
}

#[derive(Debug, PartialEq)]
enum ScheduledDispatch {
    Finished(i32),
    NotDue,
    Blocked,
}

async fn dispatch_scheduled(
    job: &Job,
    vault: &Path,
    semaphore: &tokio::sync::Semaphore,
    script_timeout: Duration,
) -> ScheduledDispatch {
    // Recheck the durable ledger while holding the per-job cross-process
    // flock, then publish the fence before waiting for capacity. A second
    // daemon therefore cannot launch the same due period.
    let attempt = match begin_scheduled_attempt(job) {
        Ok(ScheduledAttemptStart::Ready(attempt)) => attempt,
        Ok(ScheduledAttemptStart::NotDue) => return ScheduledDispatch::NotDue,
        Ok(ScheduledAttemptStart::Blocked(detail)) => {
            eprintln!("[job:{}] scheduled dispatch blocked: {detail}", job.name);
            return ScheduledDispatch::Blocked;
        }
        Err(error) => {
            eprintln!("[job:{}] scheduled attempt failed: {error}", job.name);
            return ScheduledDispatch::Blocked;
        }
    };
    let Ok(_permit) = semaphore.acquire().await else {
        return ScheduledDispatch::Blocked;
    };
    let started = SystemTime::now();
    let execution = execute_scheduled(job, vault, script_timeout).await;
    ScheduledDispatch::Finished(finish_execution(job, attempt, started, execution))
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
            let due = match last_record_ts(&job.name) {
                Ok(last) => record_is_due(last, job.every, epoch_now()),
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
            let vault = vault.clone();
            tokio::spawn(async move {
                let _ = dispatch_scheduled(&job, &vault, &sem, SCRIPT_BACKSTOP).await;
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
    fn compression_is_a_typed_scheduled_action() {
        assert_eq!(action_for("compress"), JobAction::InProcessCompression);
        assert_eq!(action_for("compressor"), JobAction::Script);
        assert_eq!(action_for("door-mail"), JobAction::Script);
    }

    #[test]
    fn recovery_requires_the_matching_ledger_descriptor_to_sync() {
        let root = std::env::temp_dir().join(format!("plant-ledger-sync-{}", uuid::Uuid::new_v4()));
        let attempts = root.join("attempts");
        let ledgers = root.join("ledgers");
        std::fs::create_dir_all(&attempts).unwrap();
        std::fs::create_dir_all(&ledgers).unwrap();
        let ledger = ledgers.join("job.jsonl");
        let fence_path = attempts.join("job.json");
        let fence = AttemptFence {
            id: "written-but-not-durable".to_string(),
            started_ts: 1,
            retryable: false,
        };
        write_fence(&fence_path, &fence).unwrap();
        let original_fence = std::fs::read(&fence_path).unwrap();
        std::fs::write(
            &ledger,
            b"{\"attempt_id\":\"written-but-not-durable\",\"outcome\":\"success\"}\n",
        )
        .unwrap();
        let error = reconcile_fence_at(&fence_path, &attempts, &ledger, &ledgers, |_| {
            Err(io::Error::other("injected sync failure"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "injected sync failure");
        assert_eq!(std::fs::read(&fence_path).unwrap(), original_fence);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_scheduler_contender_reconciles_then_observes_not_due() {
        let root =
            std::env::temp_dir().join(format!("plant-stale-contender-{}", uuid::Uuid::new_v4()));
        let attempts = root.join("attempts");
        let ledgers = root.join("ledgers");
        std::fs::create_dir_all(&attempts).unwrap();
        std::fs::create_dir_all(&ledgers).unwrap();
        let lock_path = attempts.join("job.lock");
        let first = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert_eq!(unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX) }, 0);
        assert_ne!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "a second scheduler cannot enter the same job lifecycle"
        );

        let ledger = ledgers.join("job.jsonl");
        let fence_path = attempts.join("job.json");
        std::fs::write(
            &ledger,
            b"{\"ts\":100,\"attempt_id\":\"completed\",\"outcome\":\"success\"}\n",
        )
        .unwrap();
        write_fence(
            &fence_path,
            &AttemptFence {
                id: "completed".to_string(),
                started_ts: 99,
                retryable: false,
            },
        )
        .unwrap();
        assert!(matches!(
            reconcile_fence_at(&fence_path, &attempts, &ledger, &ledgers, File::sync_all,).unwrap(),
            FenceReconcile::Ready
        ));
        assert!(!fence_path.exists());
        assert!(!record_is_due(
            last_record_ts_at(&ledger).unwrap(),
            Duration::from_secs(60),
            150,
        ));

        assert_eq!(unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(matches!(
            reconcile_fence_at(&fence_path, &attempts, &ledger, &ledgers, File::sync_all,).unwrap(),
            FenceReconcile::Ready
        ));
        assert!(!record_is_due(
            last_record_ts_at(&ledger).unwrap(),
            Duration::from_secs(60),
            150,
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_line_bypasses_shell_aliases() {
        assert_eq!(
            launch_line(
                Harness::Codex,
                Some("gpt-5.6-sol"),
                Some("-c model_reasoning_effort=xhigh")
            ),
            "command codex --dangerously-bypass-approvals-and-sandbox -m 'gpt-5.6-sol' \
             -c model_reasoning_effort=xhigh"
        );
        assert_eq!(
            launch_line(Harness::ClaudeCode, Some("opus[1m]"), None),
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
