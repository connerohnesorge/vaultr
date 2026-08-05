//! SwiftBar-style job scheduler: jobs are executable scripts at
//! <vault content root>/jobs/shared/<name>.<interval>.<ext> or
//! jobs/<short hostname>/ (e.g. learn.15m.sh, door-oncall.30m.ts). Shared jobs
//! load everywhere; a host bucket loads only on that host and overrides a
//! same-named shared job. The filename carries the
//! cadence; the script is exec'd directly, so its shebang picks the interpreter
//! (bash, bun, …) — Plant maps no extensions. The script body does the work
//! itself, composing the plant/vaultr CLIs (`plant sessions eligible --claim`,
//! `plant agent run`, `vaultr validate`, …).
//! Agent-backed jobs MUST go through `plant agent run` (Herdr pane orchestration) —
//! never `claude -p`.
//! Outcomes append to ~/.local/state/plant/jobs/<name>.jsonl; the tail line is the
//! scheduling state (due when now - last.ts >= every). Exit code contract:
//! 0 = success, 75 = retry next tick without recording (EX_TEMPFAIL, e.g. herdr down),
//! anything else = failed. The job set is rescanned every tick — edits and interval
//! renames take effect without a restart. Discovery assigns the `compress`
//! cadence marker an in-process action; the listener-owning daemon is the only
//! scheduler that may run compression, and it never executes the marker script.

#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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

fn jobs_in(dir: &Path) -> Vec<Job> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
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
        .collect()
}

/// Two buckets, most-specific-wins by job name:
///   `shared/`      — every host
///   `<hostname>/`  — this host only
///
/// A host bucket overrides a same-named shared job, so a machine can specialise
/// `learn` without forking the schedule. Scripts sitting flat in `jobs/` are
/// deliberately NOT scanned: that was the pre-bucket layout, gated by a
/// `.hostname` marker that could only ever name one host, so a second machine
/// could not have jobs of its own. Both are retired — a flat cadence-named
/// script is now inert, and `jobs/AGENTS.md` says so.
fn load_jobs_at(dir: &Path, short_hostname: &str) -> Vec<Job> {
    let mut jobs = jobs_in(&dir.join("shared"));
    overlay(&mut jobs, jobs_in(&dir.join(short_hostname)));
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    jobs
}

/// Apply a more specific bucket over a less specific one, replacing by job name.
fn overlay(jobs: &mut Vec<Job>, specific: Vec<Job>) {
    for job in specific {
        match jobs.iter_mut().find(|existing| existing.name == job.name) {
            Some(existing) => *existing = job,
            None => jobs.push(job),
        }
    }
}

pub fn load_jobs() -> Vec<Job> {
    let Some(dir) = jobs_dir() else {
        return Vec::new();
    };
    let hostname = crate::otel::hostname();
    let short_hostname = hostname.split('.').next().unwrap_or(&hostname);
    load_jobs_at(&dir, short_hostname)
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
    // Stamp PLANT_AGENT=1 into the spawned agent so a nested `plant agent run` (a job-spawned
    // agent that wanders into a job script and copies its dispatch line) is refused — see the
    // guard in the AgentRun handler. Claude's Bash tool inherits the pane env, so a var prefix
    // reaches it. Codex's shell tool uses inherit="core" and strips custom vars, so inject the
    // marker via a set-override (`-c`), which wins last and only applies to this plant launch.
    let mut s = match harness {
        Harness::ClaudeCode => {
            "PLANT_AGENT=1 command claude --dangerously-skip-permissions".to_string()
        }
        // sandboxed codex blocks on its first approval prompt — background panes can't answer.
        // --dangerously-bypass-hook-trust is the same problem by a second mechanism: codex
        // requires persisted per-hook trust and prompts "Press t to trust" for any hook it has
        // not seen, which on a fresh box is every hook in the stowed ~/.codex/hooks.json. The
        // pane sits on that prompt forever, no API call is ever made, and plant reports "agent
        // reached a terminal state without a capture session id" — the machine this was
        // developed on had answered the prompt by hand months earlier, so it only ever appears
        // on a newly provisioned host. The hooks are the box's own dotfiles, which is precisely
        // the "automation that already vets hook sources" the flag documents.
        Harness::Codex => "command codex --dangerously-bypass-approvals-and-sandbox \
             --dangerously-bypass-hook-trust \
             -c 'shell_environment_policy.set.PLANT_AGENT=\"1\"' \
             -c model_reasoning_effort=xhigh"
            .to_string(),
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

/// Timestamp a script may treat as "everything before this is already handled",
/// exported as `PLANT_LAST_TS`.
///
/// This is the last SUCCESSFUL record, not the ledger tail. Watermark-style jobs
/// select their batch with it (`find -newer`, `processed_at > $since`), so a
/// failed record must not advance it: the failed attempt is precisely the one
/// that did NOT consume its window, and moving the mark to its timestamp makes
/// the next run skip work nothing ever processed. An operator `plant jobs
/// unblock` writes such a record hours after the fence was published, which is
/// how a 19h context-audit window went silently unaudited (2026-08-03).
///
/// Dueness deliberately keeps reading the tail via `last_record_ts` — a job that
/// keeps failing must still wait out its interval instead of hot-looping.
fn last_success_ts(name: &str) -> io::Result<Option<u64>> {
    last_success_ts_at(&ledger_path(name))
}

fn last_success_ts_at(path: &Path) -> io::Result<Option<u64>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    for line in text.lines().rev().filter(|l| !l.trim().is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if record.get("outcome").and_then(serde_json::Value::as_str) != Some("success") {
            continue;
        }
        return record
            .get("ts")
            .and_then(serde_json::Value::as_u64)
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "job record has no ts"));
    }
    // No success on record behaves exactly like an absent ledger: first-run
    // baseline. Scripts already handle `PLANT_LAST_TS=0` (verify baselines, the
    // find-based ones scan from the epoch and cap their batch).
    Ok(None)
}

fn record_is_due(last: Option<u64>, every: Duration, now: u64) -> bool {
    last.is_none_or(|ts| now.saturating_sub(ts) >= every.as_secs())
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct AttemptFence {
    id: String,
    started_ts: u64,
    retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    action: Option<JobAction>,
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
    ResumableCompression(AttemptFence),
    Blocked(String),
}

fn reconcile_fence_at<F>(
    name: &str,
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
        if existing.action == Some(JobAction::InProcessCompression) {
            return Ok(FenceReconcile::ResumableCompression(existing));
        }
        // No ledger record, but the attempt ID doubles as the Agent Run
        // idempotency key: a conclusive receipt proves the effect finished even
        // though the scheduler died before recording it.
        match crate::agent_run::lookup_receipt(&existing.id) {
            Ok(crate::agent_run::ReceiptLookup::Conclusive(receipt)) => {
                let (outcome, detail) = receipt
                    .ledger_outcome()
                    .expect("a conclusive receipt has a ledger outcome");
                append_ledger_record(
                    ledger_parent,
                    ledger_path,
                    &ledger_record(
                        &existing.id,
                        outcome,
                        0,
                        &format!("reconciled from agent run receipt: {detail}"),
                    ),
                )?;
            }
            // Absent stays blocking. It does NOT prove nothing ran: a job with
            // no agent dispatch never writes a receipt at all, and the
            // herdr-unavailable path deletes its claim to keep the key
            // retryable. Clearing here would re-run plain script jobs whose
            // only failure was appending the final ledger record.
            Ok(crate::agent_run::ReceiptLookup::Absent) => {
                return Ok(FenceReconcile::Blocked(format!(
                    "attempt {} has no durable final outcome; \
                     if it is abandoned, run `plant jobs unblock {name}`",
                    existing.id
                )))
            }
            Ok(crate::agent_run::ReceiptLookup::Pending { .. }) => {
                return Ok(FenceReconcile::Blocked(format!(
                    "attempt {} claimed an agent run that never finished; \
                     if its agent is gone, run `plant jobs unblock {name}`",
                    existing.id
                )))
            }
            Err(error) => {
                return Ok(FenceReconcile::Blocked(format!(
                    "attempt {} has an unreadable agent run receipt: {error}; \
                     run `plant jobs unblock {name}`",
                    existing.id
                )))
            }
        }
    }
    std::fs::remove_file(fence_path)?;
    sync_dir(attempt_parent)?;
    Ok(FenceReconcile::Ready)
}

fn reconcile_fence(name: &str) -> io::Result<FenceReconcile> {
    let ledger = ledger_path(name);
    reconcile_fence_at(
        name,
        &attempt_path(name),
        &attempt_dir(),
        &ledger,
        ledger.parent().expect("job ledger has a parent"),
        File::sync_all,
    )
}

async fn reconcile_fence_live(name: &str) -> io::Result<FenceReconcile> {
    let result = reconcile_fence(name)?;
    if !matches!(result, FenceReconcile::Blocked(_)) {
        return Ok(result);
    }
    let fence_path = attempt_path(name);
    let text = match std::fs::read_to_string(&fence_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(result),
        Err(error) => return Err(error),
    };
    let fence: AttemptFence = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if fence.retryable || fence.action == Some(JobAction::InProcessCompression) {
        return Ok(result);
    }
    let identity = match crate::agent_run::lookup_receipt(&fence.id)? {
        crate::agent_run::ReceiptLookup::Pending {
            identity: Some(identity),
        } => identity,
        _ => return Ok(result),
    };
    match crate::agent_run::recover_pending(&fence.id, &identity).await? {
        crate::agent_run::PendingRecovery::Recovered => reconcile_fence(name),
        crate::agent_run::PendingRecovery::Retained(detail) => Ok(FenceReconcile::Blocked(
            format!("attempt {} recovery retained: {detail}", fence.id),
        )),
    }
}

/// Outcome of an operator unblock request.
#[derive(Debug, PartialEq)]
pub enum Unblocked {
    NoFence,
    AlreadyClear,
    Cleared(String),
}

/// Clear a fence held by a pending or unreadable receipt, recording the
/// abandonment so the job-health sweep sees it instead of a silent job.
///
/// Takes the same attempt flock a dispatch takes, so it cannot race a live
/// tick, and refuses to force a fence ordinary reconciliation would clear.
pub fn unblock_job(name: &str) -> io::Result<Unblocked> {
    let _lock = match acquire_attempt_lock(name)? {
        AttemptLockStart::Ready(lock) => lock,
        AttemptLockStart::Blocked(detail) => {
            return Err(io::Error::other(format!("job {name} is busy: {detail}")))
        }
    };
    let fence_path = attempt_path(name);
    let Ok(text) = std::fs::read_to_string(&fence_path) else {
        return Ok(Unblocked::NoFence);
    };
    let fence: AttemptFence = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    // Never force what resolves itself — a conclusive receipt or a matching
    // ledger record must still take the ordinary reconciliation path.
    match reconcile_fence(name)? {
        FenceReconcile::Ready | FenceReconcile::ResumableCompression(_) => {
            return Ok(Unblocked::AlreadyClear)
        }
        FenceReconcile::Blocked(_) => {}
    }
    let ledger = ledger_path(name);
    append_ledger_record(
        ledger.parent().expect("job ledger has a parent"),
        &ledger,
        &ledger_record(
            &fence.id,
            "failed",
            0,
            &format!(
                "unblocked by operator: attempt {} abandoned without a durable outcome",
                fence.id
            ),
        ),
    )?;
    std::fs::remove_file(&fence_path)?;
    sync_dir(&attempt_dir())?;
    Ok(Unblocked::Cleared(fence.id))
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

fn publish_attempt(name: &str, action: JobAction, lock: File) -> io::Result<AttemptGuard> {
    let fence = AttemptFence {
        id: uuid::Uuid::new_v4().to_string(),
        started_ts: epoch_now(),
        retryable: false,
        action: Some(action),
    };
    write_fence(&attempt_path(name), &fence)?;
    Ok(AttemptGuard {
        name: name.to_string(),
        fence,
        _lock: lock,
    })
}

#[cfg(test)]
fn begin_attempt_locked(name: &str, lock: File) -> io::Result<AttemptStart> {
    verify_ledger_writable(name)?;
    match reconcile_fence(name)? {
        FenceReconcile::Ready => {
            publish_attempt(name, JobAction::Script, lock).map(AttemptStart::Ready)
        }
        FenceReconcile::ResumableCompression(fence) => Ok(AttemptStart::Blocked(format!(
            "attempt {} awaits in-process compression resume",
            fence.id
        ))),
        FenceReconcile::Blocked(detail) => Ok(AttemptStart::Blocked(detail)),
    }
}

#[cfg(test)]
fn begin_attempt(name: &str) -> io::Result<AttemptStart> {
    match acquire_attempt_lock(name)? {
        AttemptLockStart::Ready(lock) => begin_attempt_locked(name, lock),
        AttemptLockStart::Blocked(detail) => Ok(AttemptStart::Blocked(detail)),
    }
}

async fn begin_attempt_live(name: &str) -> io::Result<AttemptStart> {
    let lock = match acquire_attempt_lock(name)? {
        AttemptLockStart::Ready(lock) => lock,
        AttemptLockStart::Blocked(detail) => return Ok(AttemptStart::Blocked(detail)),
    };
    verify_ledger_writable(name)?;
    match reconcile_fence_live(name).await? {
        FenceReconcile::Ready => {
            publish_attempt(name, JobAction::Script, lock).map(AttemptStart::Ready)
        }
        FenceReconcile::ResumableCompression(fence) => Ok(AttemptStart::Blocked(format!(
            "attempt {} awaits in-process compression resume",
            fence.id
        ))),
        FenceReconcile::Blocked(detail) => Ok(AttemptStart::Blocked(detail)),
    }
}

enum ScheduledAttemptStart {
    Ready(AttemptGuard),
    NotDue,
    Blocked(String),
}

#[cfg(test)]
fn begin_scheduled_attempt(job: &Job) -> io::Result<ScheduledAttemptStart> {
    let lock = match acquire_attempt_lock(&job.name)? {
        AttemptLockStart::Ready(lock) => lock,
        AttemptLockStart::Blocked(detail) => return Ok(ScheduledAttemptStart::Blocked(detail)),
    };
    verify_ledger_writable(&job.name)?;
    match reconcile_fence(&job.name)? {
        FenceReconcile::Ready => {}
        FenceReconcile::ResumableCompression(fence) => {
            return Ok(ScheduledAttemptStart::Ready(AttemptGuard {
                name: job.name.clone(),
                fence,
                _lock: lock,
            }))
        }
        FenceReconcile::Blocked(detail) => return Ok(ScheduledAttemptStart::Blocked(detail)),
    }
    let due = record_is_due(last_record_ts(&job.name)?, job.every, epoch_now());
    if !due {
        return Ok(ScheduledAttemptStart::NotDue);
    }
    publish_attempt(&job.name, job.action, lock).map(ScheduledAttemptStart::Ready)
}

async fn begin_scheduled_attempt_live(job: &Job) -> io::Result<ScheduledAttemptStart> {
    let lock = match acquire_attempt_lock(&job.name)? {
        AttemptLockStart::Ready(lock) => lock,
        AttemptLockStart::Blocked(detail) => return Ok(ScheduledAttemptStart::Blocked(detail)),
    };
    verify_ledger_writable(&job.name)?;
    match reconcile_fence_live(&job.name).await? {
        FenceReconcile::Ready => {}
        FenceReconcile::ResumableCompression(fence) => {
            return Ok(ScheduledAttemptStart::Ready(AttemptGuard {
                name: job.name.clone(),
                fence,
                _lock: lock,
            }))
        }
        FenceReconcile::Blocked(detail) => return Ok(ScheduledAttemptStart::Blocked(detail)),
    }
    let due = record_is_due(last_record_ts(&job.name)?, job.every, epoch_now());
    if !due {
        return Ok(ScheduledAttemptStart::NotDue);
    }
    publish_attempt(&job.name, job.action, lock).map(ScheduledAttemptStart::Ready)
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

fn ledger_record(
    attempt_id: &str,
    outcome: &str,
    duration_ms: u64,
    detail: &str,
) -> serde_json::Value {
    serde_json::json!({
        "ts": epoch_now(),
        "iso": crate::capture::iso_now(),
        "attempt_id": attempt_id,
        "outcome": outcome,
        "duration_ms": duration_ms,
        "detail": detail,
    })
}

fn append_ledger_record(dir: &Path, path: &Path, rec: &serde_json::Value) -> io::Result<()> {
    ensure_dir_durable(dir)?;
    let mut line =
        serde_json::to_vec(rec).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&line)?;
    file.sync_all()?;
    sync_dir(dir)
}

fn record(
    name: &str,
    attempt_id: &str,
    outcome: &str,
    started: SystemTime,
    detail: &str,
) -> io::Result<()> {
    let duration_ms = started.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
    append_ledger_record(
        &state_dir().join("jobs"),
        &ledger_path(name),
        &ledger_record(attempt_id, outcome, duration_ms, detail),
    )?;
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

async fn execute_script_with_timeout(
    job: &Job,
    timeout: Duration,
    attempt_id: &str,
) -> JobExecution {
    // Exec directly so the shebang picks the interpreter. Linux does not provide
    // macOS's ENOEXEC shell fallback, so preserve that contract explicitly for
    // executable legacy scripts without a shebang; a missing exec bit still fails.
    let executable = std::fs::metadata(&job.path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    let has_shebang = std::fs::read(&job.path)
        .map(|body| body.starts_with(b"#!"))
        .unwrap_or(true);
    let mut cmd = if executable && !has_shebang {
        let mut shell = tokio::process::Command::new("/bin/sh");
        shell.arg(&job.path);
        shell
    } else {
        tokio::process::Command::new(&job.path)
    };
    cmd.current_dir(script_working_dir())
        .env("PATH", script_path_env())
        // Agent-backed scripts pass this to `plant agent run --idempotency-key`,
        // so a completed run leaves a receipt this attempt's fence can reconcile.
        .env("PLANT_ATTEMPT_ID", attempt_id)
        .env(
            "PLANT_LAST_TS",
            last_success_ts(&job.name)
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
    match crate::sweep::compress_sweep(vault, Duration::from_secs(3600)).await {
        Ok(()) => JobExecution::Succeeded("in-process compression complete".to_string()),
        Err(error) => JobExecution::Failed(format!("in-process compression failed: {error}")),
    }
}

async fn execute_scheduled(
    job: &Job,
    action: JobAction,
    vault: &Path,
    script_timeout: Duration,
    attempt_id: &str,
) -> JobExecution {
    match action {
        JobAction::Script => execute_script_with_timeout(job, script_timeout, attempt_id).await,
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
    let attempt = match begin_attempt_live(&job.name).await {
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
    let execution = execute_script_with_timeout(job, SCRIPT_BACKSTOP, &attempt.fence.id).await;
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
    let Ok(_permit) = semaphore.acquire().await else {
        return ScheduledDispatch::Blocked;
    };
    // Once capacity is admitted, hold the per-job cross-process flock across
    // the durable cadence recheck, fence, execution, and final transition.
    let attempt = match begin_scheduled_attempt_live(job).await {
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
    let started = SystemTime::now();
    let Some(action) = attempt.fence.action else {
        eprintln!(
            "[job:{}] scheduled dispatch blocked: attempt {} has no action kind",
            job.name, attempt.fence.id
        );
        return ScheduledDispatch::Blocked;
    };
    let execution = execute_scheduled(job, action, vault, script_timeout, &attempt.fence.id).await;
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

    /// Receipts are keyed by sha256 of the attempt id; mirror that here rather
    /// than exporting the private helper.
    fn write_receipt(state: &Path, key: &str, body: &str) {
        use sha2::{Digest, Sha256};
        let dir = state.join("agent-runs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))),
            body,
        )
        .unwrap();
    }

    fn fenced_job(tag: &str, id: &str) -> (PathBuf, crate::state::TestDirGuard) {
        let root = std::env::temp_dir().join(format!("plant-{tag}-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        std::fs::create_dir_all(state.join("job-attempts")).unwrap();
        std::fs::create_dir_all(state.join("jobs")).unwrap();
        let guard = crate::state::use_test_dir(state.clone());
        write_fence(
            &attempt_path("j"),
            &AttemptFence {
                id: id.to_string(),
                started_ts: 1,
                retryable: false,
                action: None,
            },
        )
        .unwrap();
        (state, guard)
    }

    #[test]
    fn absent_receipt_keeps_blocking_but_names_the_remedy() {
        let (state, _guard) = fenced_job("fence-absent", "never-recorded");
        // The incident shape. Absent must NOT auto-clear: a job that dispatches
        // no agent never writes a receipt, so clearing would re-run a script
        // whose only failure was appending its final ledger record.
        let FenceReconcile::Blocked(detail) = reconcile_fence("j").unwrap() else {
            panic!("an unproven outcome must not silently re-dispatch");
        };
        assert!(detail.contains("plant jobs unblock j"), "{detail}");
        assert!(attempt_path("j").exists());
        std::fs::remove_dir_all(state.parent().unwrap()).unwrap();
    }

    #[test]
    fn pending_receipt_still_blocks_and_names_the_unblock_command() {
        let (state, _guard) = fenced_job("fence-pending", "claimed-never-finished");
        write_receipt(
            &state,
            "claimed-never-finished",
            r#"{"state":"in_progress","key":"claimed-never-finished"}"#,
        );
        let FenceReconcile::Blocked(detail) = reconcile_fence("j").unwrap() else {
            panic!("a claimed run with no outcome is genuinely ambiguous and must block");
        };
        assert!(detail.contains("plant jobs unblock j"), "{detail}");
        assert!(attempt_path("j").exists());
        std::fs::remove_dir_all(state.parent().unwrap()).unwrap();
    }

    #[test]
    fn conclusive_receipt_still_reconciles_into_a_ledger_record() {
        let (state, _guard) = fenced_job("fence-conclusive", "finished");
        write_receipt(
            &state,
            "finished",
            r#"{"state":"succeeded","key":"finished","detail":"agent done"}"#,
        );
        assert!(matches!(
            reconcile_fence("j").unwrap(),
            FenceReconcile::Ready
        ));
        let ledger = std::fs::read_to_string(state.join("jobs/j.jsonl")).unwrap();
        assert!(
            ledger.contains("reconciled from agent run receipt"),
            "{ledger}"
        );
        std::fs::remove_dir_all(state.parent().unwrap()).unwrap();
    }

    #[test]
    fn unblock_clears_an_abandoned_fence_with_one_failed_record() {
        let (state, _guard) = fenced_job("unblock-pending", "abandoned");
        write_receipt(
            &state,
            "abandoned",
            r#"{"state":"in_progress","key":"abandoned"}"#,
        );
        assert_eq!(
            unblock_job("j").unwrap(),
            Unblocked::Cleared("abandoned".to_string())
        );
        assert!(!attempt_path("j").exists());
        let lines: Vec<_> = std::fs::read_to_string(state.join("jobs/j.jsonl"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly one record so health sees one failure"
        );
        assert!(lines[0].contains(r#""outcome":"failed""#), "{}", lines[0]);
        assert!(
            lines[0].contains("abandoned without a durable outcome"),
            "{}",
            lines[0]
        );
        std::fs::remove_dir_all(state.parent().unwrap()).unwrap();
    }

    #[test]
    fn unblock_refuses_to_force_a_self_resolving_fence() {
        let (state, _guard) = fenced_job("unblock-resolving", "finished");
        write_receipt(
            &state,
            "finished",
            r#"{"state":"succeeded","key":"finished","detail":"agent done"}"#,
        );
        assert_eq!(unblock_job("j").unwrap(), Unblocked::AlreadyClear);
        let ledger = std::fs::read_to_string(state.join("jobs/j.jsonl")).unwrap();
        assert!(
            !ledger.contains("unblocked by operator"),
            "reconciliation owns this fence, not the operator: {ledger}"
        );
        std::fs::remove_dir_all(state.parent().unwrap()).unwrap();
    }

    #[test]
    fn unblock_without_a_fence_is_a_harmless_no_op() {
        let root =
            std::env::temp_dir().join(format!("plant-unblock-none-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        std::fs::create_dir_all(state.join("job-attempts")).unwrap();
        std::fs::create_dir_all(state.join("jobs")).unwrap();
        let _guard = crate::state::use_test_dir(state.clone());
        assert_eq!(unblock_job("j").unwrap(), Unblocked::NoFence);
        assert!(!state.join("jobs/j.jsonl").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

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

    /// The retired flat bucket must stay retired. A cadence-named script left at
    /// `jobs/` top level is inert on every host, and a leftover `.hostname` marker
    /// grants it nothing — this is the regression that would silently resurrect
    /// one host's private schedule on every machine.
    #[test]
    fn flat_scripts_and_a_stale_hostname_marker_are_both_ignored() {
        let root = std::env::temp_dir().join(format!("plant-job-host-{}", uuid::Uuid::new_v4()));
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(root.join("local.1h.sh"), "").unwrap();
        std::fs::write(shared.join("portable.1h.sh"), "").unwrap();

        let names = |host| {
            load_jobs_at(&root, host)
                .into_iter()
                .map(|job| job.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("other"), ["portable"]);

        std::fs::write(root.join(".hostname"), "CB14957\n").unwrap();
        assert_eq!(names("CB14957"), ["portable"]);
        assert_eq!(names("allocator-vm"), ["portable"]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_bucket_adds_jobs_and_overrides_less_specific_buckets() {
        let root = std::env::temp_dir().join(format!("plant-job-bucket-{}", uuid::Uuid::new_v4()));
        let shared = root.join("shared");
        let vm = root.join("allocator-vm");
        let mac = root.join("CB14957");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&vm).unwrap();
        std::fs::create_dir_all(&mac).unwrap();
        std::fs::write(mac.join("door-mail.30m.ts"), "").unwrap();
        std::fs::write(shared.join("health.15m.sh"), "").unwrap();
        std::fs::write(vm.join("learn.3h.sh"), "").unwrap();
        // same name as a shared job: the host bucket wins, it does not duplicate
        std::fs::write(vm.join("health.5m.sh"), "").unwrap();

        let jobs = |host| load_jobs_at(&root, host);
        let names = |host| {
            jobs(host)
                .into_iter()
                .map(|job| job.name)
                .collect::<Vec<_>>()
        };

        // The VM never sees the Mac's jobs, and gets its own learner.
        assert_eq!(names("allocator-vm"), ["health", "learn"]);
        let health = jobs("allocator-vm")
            .into_iter()
            .find(|job| job.name == "health")
            .unwrap();
        assert_eq!(health.every, Duration::from_secs(300));
        assert_eq!(health.path, vm.join("health.5m.sh"));

        // The Mac gets its own bucket's job plus the un-overridden shared one.
        assert_eq!(names("CB14957"), ["door-mail", "health"]);
        let mac_health = jobs("CB14957")
            .into_iter()
            .find(|job| job.name == "health")
            .unwrap();
        assert_eq!(mac_health.every, Duration::from_secs(900));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compression_is_a_typed_scheduled_action() {
        assert_eq!(action_for("compress"), JobAction::InProcessCompression);
        assert_eq!(action_for("compressor"), JobAction::Script);
        assert_eq!(action_for("door-mail"), JobAction::Script);
    }

    #[test]
    fn scheduled_attempt_persists_the_discovered_action() {
        let root =
            std::env::temp_dir().join(format!("plant-scheduled-action-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        let _state = crate::state::use_test_dir(state.clone());
        let job = Job {
            name: "compress".to_string(),
            path: root.join("compress.30m.sh"),
            every: Duration::from_secs(1800),
            action: JobAction::InProcessCompression,
        };

        let ScheduledAttemptStart::Ready(attempt) = begin_scheduled_attempt(&job).unwrap() else {
            panic!("a first scheduled attempt must be due");
        };
        assert_eq!(attempt.fence.action, Some(JobAction::InProcessCompression));
        let persisted: AttemptFence =
            serde_json::from_slice(&std::fs::read(attempt_path("compress")).unwrap()).unwrap();
        assert_eq!(persisted.action, Some(JobAction::InProcessCompression));

        drop(attempt);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_compression_attempt_persists_script_action() {
        let root = std::env::temp_dir().join(format!(
            "plant-manual-compression-action-{}",
            uuid::Uuid::new_v4()
        ));
        let state = root.join("state");
        let _state = crate::state::use_test_dir(state.clone());

        let AttemptStart::Ready(attempt) = begin_attempt("compress").unwrap() else {
            panic!("a first manual attempt must be ready");
        };
        assert_eq!(attempt.fence.action, Some(JobAction::Script));
        let persisted: AttemptFence =
            serde_json::from_slice(&std::fs::read(attempt_path("compress")).unwrap()).unwrap();
        assert_eq!(persisted.action, Some(JobAction::Script));

        drop(attempt);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_actionless_fence_deserializes_without_identity() {
        let fence: AttemptFence =
            serde_json::from_str(r#"{"id":"legacy","started_ts":1,"retryable":false}"#).unwrap();

        assert_eq!(fence.id, "legacy");
        assert_eq!(fence.action, None);
    }

    #[test]
    fn typed_script_fence_still_reconciles_from_its_agent_run_receipt() {
        let root = std::env::temp_dir().join(format!(
            "plant-script-receipt-reconcile-{}",
            uuid::Uuid::new_v4()
        ));
        let state = root.join("state");
        std::fs::create_dir_all(state.join("job-attempts")).unwrap();
        std::fs::create_dir_all(state.join("jobs")).unwrap();
        let _state = crate::state::use_test_dir(state.clone());
        write_fence(
            &attempt_path("script"),
            &AttemptFence {
                id: "typed-script-finished".to_string(),
                started_ts: 1,
                retryable: false,
                action: Some(JobAction::Script),
            },
        )
        .unwrap();
        write_receipt(
            &state,
            "typed-script-finished",
            r#"{"state":"succeeded","key":"typed-script-finished","detail":"agent done"}"#,
        );

        assert!(matches!(
            reconcile_fence("script").unwrap(),
            FenceReconcile::Ready
        ));
        let ledger = std::fs::read_to_string(state.join("jobs/script.jsonl")).unwrap();
        assert!(
            ledger.contains("reconciled from agent run receipt"),
            "{ledger}"
        );
        assert!(!attempt_path("script").exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_compression_fence_requires_operator_recovery() {
        let root = std::env::temp_dir().join(format!(
            "plant-legacy-compression-fence-{}",
            uuid::Uuid::new_v4()
        ));
        let state = root.join("state");
        std::fs::create_dir_all(state.join("job-attempts")).unwrap();
        std::fs::create_dir_all(state.join("jobs")).unwrap();
        let _state = crate::state::use_test_dir(state.clone());
        write_fence(
            &attempt_path("compress"),
            &AttemptFence {
                id: "legacy-compression".to_string(),
                started_ts: 1,
                retryable: false,
                action: None,
            },
        )
        .unwrap();
        let job = Job {
            name: "compress".to_string(),
            path: root.join("compress.30m.sh"),
            every: Duration::from_secs(1800),
            action: JobAction::InProcessCompression,
        };

        let ScheduledAttemptStart::Blocked(detail) = begin_scheduled_attempt(&job).unwrap() else {
            panic!("legacy actionless compression must stay blocked");
        };
        assert!(detail.contains("plant jobs unblock compress"), "{detail}");
        assert!(attempt_path("compress").exists());

        assert_eq!(
            unblock_job("compress").unwrap(),
            Unblocked::Cleared("legacy-compression".to_string())
        );
        let ledger = std::fs::read_to_string(state.join("jobs/compress.jsonl")).unwrap();
        assert!(
            ledger.contains(r#""attempt_id":"legacy-compression""#),
            "{ledger}"
        );
        assert!(ledger.contains(r#""outcome":"failed""#), "{ledger}");
        assert!(!attempt_path("compress").exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_compression_fence_returns_its_existing_attempt_guard() {
        let root = std::env::temp_dir().join(format!(
            "plant-resumable-compression-{}",
            uuid::Uuid::new_v4()
        ));
        let state = root.join("state");
        std::fs::create_dir_all(state.join("job-attempts")).unwrap();
        let _state = crate::state::use_test_dir(state.clone());
        write_fence(
            &attempt_path("compress"),
            &AttemptFence {
                id: "original-compression-attempt".to_string(),
                started_ts: 1,
                retryable: false,
                action: Some(JobAction::InProcessCompression),
            },
        )
        .unwrap();
        let original_fence = std::fs::read(attempt_path("compress")).unwrap();
        let job = Job {
            name: "compress".to_string(),
            path: root.join("compress.30m.sh"),
            every: Duration::from_secs(1800),
            action: JobAction::InProcessCompression,
        };

        let ScheduledAttemptStart::Ready(attempt) = begin_scheduled_attempt(&job).unwrap() else {
            panic!("typed compression must resume");
        };
        assert_eq!(attempt.fence.id, "original-compression-attempt");
        assert_eq!(attempt.fence.action, Some(JobAction::InProcessCompression));
        assert!(attempt_path("compress").exists());
        assert_eq!(
            std::fs::read(attempt_path("compress")).unwrap(),
            original_fence,
            "resume must not publish a replacement fence"
        );

        assert_eq!(
            finish_execution(
                &job,
                attempt,
                SystemTime::now(),
                JobExecution::Succeeded("resumed compression complete".to_string()),
            ),
            0
        );
        let ledger = std::fs::read_to_string(state.join("jobs/compress.jsonl")).unwrap();
        let records: Vec<serde_json::Value> = ledger
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["attempt_id"], "original-compression-attempt");
        assert!(!attempt_path("compress").exists());

        std::fs::remove_dir_all(root).unwrap();
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
            action: None,
        };
        write_fence(&fence_path, &fence).unwrap();
        let original_fence = std::fs::read(&fence_path).unwrap();
        std::fs::write(
            &ledger,
            b"{\"attempt_id\":\"written-but-not-durable\",\"outcome\":\"success\"}\n",
        )
        .unwrap();
        let error = reconcile_fence_at("t", &fence_path, &attempts, &ledger, &ledgers, |_| {
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
                action: None,
            },
        )
        .unwrap();
        assert!(matches!(
            reconcile_fence_at(
                "t",
                &fence_path,
                &attempts,
                &ledger,
                &ledgers,
                File::sync_all,
            )
            .unwrap(),
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
            reconcile_fence_at(
                "t",
                &fence_path,
                &attempts,
                &ledger,
                &ledgers,
                File::sync_all,
            )
            .unwrap(),
            FenceReconcile::Ready
        ));
        assert!(!record_is_due(
            last_record_ts_at(&ledger).unwrap(),
            Duration::from_secs(60),
            150,
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Build a ledger file from (ts, outcome) pairs and hand back its path.
    fn ledger_of(tag: &str, records: &[(u64, &str)]) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("plant-watermark-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let ledger = root.join("j.jsonl");
        let body: String = records
            .iter()
            .map(|(ts, outcome)| {
                format!(
                    r#"{{"ts":{ts},"iso":"x","attempt_id":"a{ts}","outcome":"{outcome}","duration_ms":0,"detail":"d"}}"#
                ) + "\n"
            })
            .collect();
        std::fs::write(&ledger, body).unwrap();
        (root, ledger)
    }

    /// The regression: an abandoned attempt recorded `failed` long after its
    /// fence was published must not carry the watermark forward over the window
    /// it never processed.
    #[test]
    fn unblock_failure_does_not_advance_the_watermark() {
        let (root, ledger) = ledger_of(
            "unblock",
            &[(100, "success"), (200, "success"), (999, "failed")],
        );
        assert_eq!(last_success_ts_at(&ledger).unwrap(), Some(200));
        // Dueness still reads the tail, so a job that keeps failing waits out
        // its interval instead of hot-looping.
        assert_eq!(last_record_ts_at(&ledger).unwrap(), Some(999));
        assert!(!record_is_due(
            last_record_ts_at(&ledger).unwrap(),
            Duration::from_secs(60),
            1000,
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watermark_is_the_tail_when_the_tail_succeeded() {
        let (root, ledger) = ledger_of("tail", &[(100, "failed"), (200, "success")]);
        assert_eq!(last_success_ts_at(&ledger).unwrap(), Some(200));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A ledger with nothing but failures is indistinguishable from a job that
    /// has never consumed a window, so it baselines like a first run.
    #[test]
    fn watermark_without_any_success_is_a_first_run() {
        let (root, ledger) = ledger_of("nosuccess", &[(100, "failed"), (200, "failed")]);
        assert_eq!(last_success_ts_at(&ledger).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watermark_of_an_absent_ledger_is_a_first_run() {
        let missing =
            std::env::temp_dir().join(format!("plant-watermark-gone-{}", uuid::Uuid::new_v4()));
        assert_eq!(last_success_ts_at(&missing).unwrap(), None);
    }

    #[test]
    fn launch_line_bypasses_shell_aliases() {
        assert_eq!(
            launch_line(
                Harness::Codex,
                Some("gpt-5.6-sol"),
                Some("-c model_reasoning_effort=high")
            ),
            "command codex --dangerously-bypass-approvals-and-sandbox \
             --dangerously-bypass-hook-trust \
             -c 'shell_environment_policy.set.PLANT_AGENT=\"1\"' \
             -c model_reasoning_effort=xhigh -m 'gpt-5.6-sol' \
             -c model_reasoning_effort=high"
        );
        assert_eq!(
            launch_line(Harness::ClaudeCode, Some("opus[1m]"), None),
            "PLANT_AGENT=1 command claude --dangerously-skip-permissions --model='opus[1m]'"
        );
    }

    /// Every prompt codex can raise before its first API call is fatal to a background pane:
    /// nobody can answer it, so plant only ever sees a terminal state with no capture session.
    /// A host where a human once answered the prompt by hand hides this completely, so assert
    /// the bypasses are on the launch line rather than trusting local state.
    #[test]
    fn codex_launch_answers_every_prompt_a_background_pane_cannot() {
        let line = launch_line(Harness::Codex, None, None);
        for flag in [
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-hook-trust",
        ] {
            assert!(line.contains(flag), "{flag} missing from: {line}");
        }
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
