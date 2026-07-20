//! Session inventory, eligibility policy, and maintenance orchestration.
//! Capture owns scrubbing, detachment, and exact-once sealing; jobs owns
//! scheduling. Operational sealing failures remain explicit.

use crate::domain::Harness;
use crate::process::{run, run30, which};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Session id to latest `processed_at` for one learner. Entries without a
/// learner key are the legacy Claude pass.
fn ledger_latest(vault: &Path, learner: Harness) -> HashMap<String, u64> {
    let mut processed = HashMap::new();
    let Ok(root) = vaultr::validate::content_root(vault) else {
        return processed;
    };
    let Ok(text) = std::fs::read_to_string(vaultr::validate::ledger_path(&root)) else {
        return processed;
    };
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let recorded = value.get("learner").and_then(|learner| learner.as_str());
        if recorded != Some(learner.ledger_label())
            && (recorded.is_some() || learner != Harness::ClaudeCode)
        {
            continue;
        }
        let Some(sid) = value.get("session_id").and_then(|sid| sid.as_str()) else {
            continue;
        };
        let timestamp = value
            .get("processed_at")
            .and_then(|timestamp| timestamp.as_str())
            .and_then(iso_to_epoch)
            .unwrap_or(0);
        let prior = processed.entry(sid.to_string()).or_insert(0);
        *prior = (*prior).max(timestamp);
    }
    processed
}

/// ISO-8601 seconds (fractional seconds tolerated) to Unix epoch seconds.
fn iso_to_epoch(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let (year, month) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month - 3) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationKind {
    Raw,
    Sealed,
    Detached,
}

#[derive(Clone, Debug)]
struct SessionGeneration {
    sid: String,
    inventory: vaultr::vault::CaptureGenerations,
    selected: GenerationKind,
}

impl SessionGeneration {
    fn current(sid: String, inventory: vaultr::vault::CaptureGenerations) -> Option<Self> {
        let selected = if inventory.raw.is_some() {
            GenerationKind::Raw
        } else if inventory.sealed.is_some() {
            GenerationKind::Sealed
        } else if inventory.detached.is_some() {
            GenerationKind::Detached
        } else {
            return None;
        };
        Some(Self {
            sid,
            inventory,
            selected,
        })
    }

    fn pending_seal(sid: String, inventory: vaultr::vault::CaptureGenerations) -> Option<Self> {
        let selected = if inventory.detached.is_some() {
            GenerationKind::Detached
        } else if inventory.raw.is_some() {
            GenerationKind::Raw
        } else {
            return None;
        };
        Some(Self {
            sid,
            inventory,
            selected,
        })
    }

    fn path(&self) -> &Path {
        match self.selected {
            GenerationKind::Raw => self.inventory.raw.as_deref(),
            GenerationKind::Sealed => self.inventory.sealed.as_deref(),
            GenerationKind::Detached => self
                .inventory
                .detached
                .as_ref()
                .map(|generation| generation.path.as_path()),
        }
        .expect("selected capture generation is present")
    }

    fn learned_current(&self, latest: &HashMap<String, u64>) -> Result<bool, String> {
        let Some(&timestamp) = latest.get(&self.sid) else {
            return Ok(false);
        };
        if self.selected != GenerationKind::Raw {
            return Ok(true);
        }
        let Some(previous) = self
            .inventory
            .detached
            .as_ref()
            .map(|generation| generation.path.as_path())
            .or(self.inventory.sealed.as_deref())
        else {
            return Ok(true);
        };
        let modified = std::fs::metadata(previous)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| {
                format!(
                    "inspect prior capture generation {}: {error}",
                    previous.display()
                )
            })?;
        let Some(boundary) = modified.duration_since(std::time::UNIX_EPOCH).ok() else {
            return Ok(true);
        };
        Ok(timestamp > boundary.as_secs())
    }

    fn idle_secs(&self) -> Result<Option<u64>, String> {
        let modified = std::fs::metadata(self.path())
            .and_then(|metadata| metadata.modified())
            .map_err(|error| {
                format!(
                    "inspect capture generation {}: {error}",
                    self.path().display()
                )
            })?;
        Ok(SystemTime::now()
            .duration_since(modified)
            .ok()
            .map(|duration| duration.as_secs()))
    }

    fn idle_for(&self, idle: Duration) -> Result<bool, String> {
        Ok(self
            .idle_secs()?
            .is_some_and(|seconds| seconds >= idle.as_secs()))
    }

    fn substantive(&self) -> Result<bool, String> {
        if self.selected != GenerationKind::Raw {
            return Ok(true);
        }
        let size = std::fs::metadata(self.path())
            .map(|metadata| metadata.len())
            .map_err(|error| {
                format!(
                    "inspect capture generation {}: {error}",
                    self.path().display()
                )
            })?;
        if size > 20_480 {
            return Ok(true);
        }
        std::fs::read_to_string(self.path())
            .map(|text| text.trim_end().lines().count() > 5)
            .map_err(|error| format!("read capture generation {}: {error}", self.path().display()))
    }

    fn ready_to_seal(
        &self,
        claude: &HashMap<String, u64>,
        codex: &HashMap<String, u64>,
        jobs: &HashSet<String>,
        idle: Duration,
    ) -> Result<bool, String> {
        if self.selected == GenerationKind::Detached {
            return Ok(true);
        }
        Ok(
            (self.learned_current(claude)? && self.learned_current(codex)?
                || jobs.contains(&self.sid))
                && self.idle_for(idle)?,
        )
    }
}

fn session_generations(
    vault: &Path,
    select: fn(String, vaultr::vault::CaptureGenerations) -> Option<SessionGeneration>,
) -> Result<Vec<SessionGeneration>, String> {
    let mut generations = Vec::new();
    let sessions = vaultr::vault::walk_session_dirs(vault).map_err(|error| error.to_string())?;
    for (sid, session) in sessions {
        let inventory =
            vaultr::vault::CaptureGenerations::load(&session).map_err(|error| error.to_string())?;
        if let Some(generation) = select(sid, inventory) {
            generations.push(generation);
        }
    }
    Ok(generations)
}

fn current_generations(vault: &Path) -> Result<Vec<SessionGeneration>, String> {
    session_generations(vault, SessionGeneration::current)
}

fn pending_generations(vault: &Path) -> Result<Vec<SessionGeneration>, String> {
    session_generations(vault, SessionGeneration::pending_seal)
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn inflight_path(vault: &Path, learner: Harness) -> Result<PathBuf, String> {
    let root = vaultr::validate::content_root(vault).map_err(|error| error.to_string())?;
    Ok(root
        .join("learnings")
        .join(format!(".inflight-{}.json", learner.ledger_label())))
}

#[derive(Debug, Serialize, Deserialize)]
struct InflightLease {
    sids: Vec<String>,
    expires_at: u64,
}

fn read_inflight(path: &Path) -> Result<Option<InflightLease>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let lease: InflightLease = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if epoch_now() >= lease.expires_at {
        return Ok(None);
    }
    if lease.sids.is_empty() {
        return Err(format!("{} has an empty active batch", path.display()));
    }
    Ok(Some(lease))
}

fn inflight_sessions(vault: &Path, learner: Harness) -> HashSet<String> {
    let Ok(path) = inflight_path(vault, learner) else {
        return HashSet::new();
    };
    read_inflight(&path)
        .ok()
        .flatten()
        .map(|lease| lease.sids.into_iter().collect())
        .unwrap_or_default()
}

fn publish_inflight(path: &Path, lease: &InflightLease) -> Result<(), String> {
    let body = serde_json::to_vec(lease).map_err(|error| error.to_string())?;
    crate::fsutil::atomic_replace(path, &body)
        .map_err(|error| format!("publish {}: {error}", path.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StuckState {
    SealBlocked,
    HalfLearned(Harness),
    Unlearned,
    SubThreshold,
    JobCapture,
}

impl StuckState {
    const REPORT_ORDER: [Self; 6] = [
        Self::SealBlocked,
        Self::HalfLearned(Harness::ClaudeCode),
        Self::HalfLearned(Harness::Codex),
        Self::Unlearned,
        Self::SubThreshold,
        Self::JobCapture,
    ];

    pub fn is_actionable(self) -> bool {
        matches!(
            self,
            Self::SealBlocked | Self::HalfLearned(_) | Self::Unlearned
        )
    }
}

impl fmt::Display for StuckState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SealBlocked => formatter.write_str("seal-blocked"),
            Self::HalfLearned(learner) => {
                write!(formatter, "half-learned:{}", learner.ledger_label())
            }
            Self::Unlearned => formatter.write_str("unlearned"),
            Self::SubThreshold => formatter.write_str("sub-threshold"),
            Self::JobCapture => formatter.write_str("job-capture"),
        }
    }
}

pub struct StuckCapture {
    pub sid: String,
    pub state: StuckState,
    pub idle_secs: u64,
}

pub fn stuck_summary(stuck: &[StuckCapture]) -> String {
    format!(
        "sessions-stuck summary: {}",
        StuckState::REPORT_ORDER
            .iter()
            .map(|state| format!(
                "{state}={}",
                stuck
                    .iter()
                    .filter(|capture| capture.state == *state)
                    .count()
            ))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

pub fn stuck_captures(vault: &Path, age: Duration) -> Result<Vec<StuckCapture>, String> {
    let claude = ledger_latest(vault, Harness::ClaudeCode);
    let codex = ledger_latest(vault, Harness::Codex);
    let jobs = job_sids();
    let mut stuck = Vec::new();
    for generation in pending_generations(vault)? {
        let Some(idle_secs) = generation.idle_secs()? else {
            continue;
        };
        if idle_secs < age.as_secs() {
            continue;
        }
        let state = if jobs.contains(&generation.sid) {
            StuckState::JobCapture
        } else {
            match (
                generation.learned_current(&claude)?,
                generation.learned_current(&codex)?,
            ) {
                (true, true) => StuckState::SealBlocked,
                (true, false) => StuckState::HalfLearned(Harness::Codex),
                (false, true) => StuckState::HalfLearned(Harness::ClaudeCode),
                (false, false) if generation.substantive()? => StuckState::Unlearned,
                (false, false) => StuckState::SubThreshold,
            }
        };
        stuck.push(StuckCapture {
            sid: generation.sid,
            state,
            idle_secs,
        });
    }
    Ok(stuck)
}

pub fn job_sids_path() -> PathBuf {
    crate::state::dir().join("job-sids.txt")
}

pub fn job_sids() -> HashSet<String> {
    job_sids_at(&job_sids_path())
}

fn job_sids_at(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn register_job_sid(sid: &str) {
    let path = job_sids_path();
    let parent = path.parent().expect("job SID path has a parent");
    if crate::state::ensure_dir_durable(parent).is_err() {
        return;
    }
    use std::io::Write;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(file, "{sid}")
        .and_then(|_| file.sync_all())
        .and_then(|_| crate::state::sync_dir(parent));
}

fn eligible_candidates(
    vault: &Path,
    idle: Duration,
    max: usize,
    learner: Harness,
) -> Result<Vec<(String, PathBuf)>, String> {
    let processed = ledger_latest(vault, learner);
    let inflight = inflight_sessions(vault, learner);
    let jobs = job_sids();
    let mut candidates = Vec::new();
    for generation in current_generations(vault)? {
        if jobs.contains(&generation.sid)
            || generation.learned_current(&processed)?
            || inflight.contains(&generation.sid)
            || !generation.idle_for(idle)?
            || !generation.substantive()?
        {
            continue;
        }
        let Some(directory) = generation.path().parent().map(Path::to_path_buf) else {
            continue;
        };
        candidates.push((generation.sid, directory));
    }
    candidates.truncate(max);
    Ok(candidates)
}

pub fn eligible_sessions(
    vault: &Path,
    idle: Duration,
    max: usize,
    learner: Harness,
) -> Result<Vec<PathBuf>, String> {
    Ok(eligible_candidates(vault, idle, max, learner)?
        .into_iter()
        .map(|(_, path)| path)
        .collect())
}

/// Select and atomically lease one batch while holding a learner-scoped
/// cross-process lock. Malformed or unpublishable state fails closed.
pub fn eligible_and_claim(
    vault: &Path,
    idle: Duration,
    max: usize,
    learner: Harness,
    duration: Duration,
) -> Result<Vec<PathBuf>, String> {
    let path = inflight_path(vault, learner)?;
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
    lock.lock()
        .map_err(|error| format!("lock {}: {error}", lock_path.display()))?;
    if read_inflight(&path)?.is_some() {
        return Ok(Vec::new());
    }
    let batch = eligible_candidates(vault, idle, max, learner)?;
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let lease = InflightLease {
        sids: batch.iter().map(|(sid, _)| sid.clone()).collect(),
        expires_at: epoch_now()
            .saturating_add(duration.as_secs())
            .saturating_add(300),
    };
    publish_inflight(&path, &lease)?;
    Ok(batch.into_iter().map(|(_, path)| path).collect())
}

pub fn eligibility_stats(vault: &Path, learner: Harness) -> Result<(usize, usize), String> {
    Ok((
        current_generations(vault)?.len(),
        ledger_latest(vault, learner).len(),
    ))
}

const COMMIT_CAP: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum CompressError {
    Inventory(String),
    Operational(String),
}

impl fmt::Display for CompressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inventory(error) | Self::Operational(error) => formatter.write_str(error),
        }
    }
}

fn exclude_from_commit(vault: &Path, sealed: &Path, size: u64) {
    let Ok(root) = vaultr::validate::content_root(vault) else {
        return;
    };
    let Ok(relative) = sealed.strip_prefix(&root) else {
        return;
    };
    let line = relative.display().to_string();
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if existing.lines().any(|entry| entry.trim() == line) {
        return;
    }
    use std::io::Write;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore)
    else {
        return;
    };
    let _ = writeln!(
        file,
        "# oversized seal (auto, {:.1}GB): kept on disk, excluded from git\n{line}",
        size as f64 / 1e9
    );
    println!(
        "[compress] {line}: {:.1}GB exceeds commit cap, gitignored",
        size as f64 / 1e9
    );
}

pub async fn compress_sweep(vault: &Path, idle: Duration) -> Result<(), CompressError> {
    let pending = pending_generations(vault).map_err(CompressError::Inventory)?;
    if !which("zstd") {
        return Err(CompressError::Operational("zstd not on PATH".into()));
    }
    let claude = ledger_latest(vault, Harness::ClaudeCode);
    let codex = ledger_latest(vault, Harness::Codex);
    let jobs = job_sids();
    let mut sealed = 0u32;
    for selected in pending {
        if !selected
            .ready_to_seal(&claude, &codex, &jobs, idle)
            .map_err(CompressError::Inventory)?
        {
            continue;
        }
        let directory = selected.path().parent().ok_or_else(|| {
            CompressError::Inventory(format!(
                "capture has no session directory: {}",
                selected.path().display()
            ))
        })?;
        let Some(generation) =
            crate::capture::seal_ready_generation(vault, &selected.sid, directory)
                .await
                .map_err(CompressError::Operational)?
        else {
            continue;
        };
        sealed += 1;
        let sealed_size = std::fs::metadata(&generation.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if sealed_size > COMMIT_CAP {
            exclude_from_commit(vault, &generation.path, sealed_size);
        }
        let path = generation.path.display().to_string();
        let relative = path.split("/sessions/").nth(1).unwrap_or(&path);
        println!(
            "[compress] {relative}: {:.1}MB -> {:.1}MB",
            generation.source_len as f64 / 1e6,
            sealed_size as f64 / 1e6
        );
    }
    if sealed == 0 {
        println!("[compress] nothing to seal");
        return Ok(());
    }
    let repository = match vaultr::validate::content_root(vault) {
        Ok(path) if path.as_os_str().is_empty() => ".".to_string(),
        Ok(path) => path.to_str().unwrap_or(".").to_string(),
        Err(_) => {
            println!("[compress] sealed {sealed}, commit skipped: sessions root has no parent");
            return Ok(());
        }
    };
    run30(&["git", "-C", &repository, "add", "-A", "sessions"]).await;
    let message = format!("chore: seal {sealed} session(s) (scrubbed + zstd)");
    run30(&["git", "-C", &repository, "commit", "-m", &message]).await;
    let push = run(
        &["git", "-C", &repository, "push"],
        Duration::from_secs(300),
    )
    .await;
    println!(
        "[compress] sealed {sealed}, push {}",
        if push.ok {
            "ok"
        } else {
            "FAILED (next sweep retries)"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests;
