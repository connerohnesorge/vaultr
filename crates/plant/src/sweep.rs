//! Session-sweep primitives: eligibility discovery, scrub, compress. Orchestration
//! (scheduling, agent panes) lives in jobs.rs; these are exposed as `plant sessions
//! eligible` / `plant compress once` subcommands and called directly by the built-in Rust jobs.
//! Every failure path non-fatal: capture uptime is sacred. All heavy work shells out.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

/// How a subprocess run ended. Distinguishes the cases `ok: false` collapses:
/// a timeout, a spawn error (binary missing from PATH), and a non-zero exit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RunEnd {
    /// The process ran to completion; `None` code means killed by a signal.
    Exited(Option<i32>),
    TimedOut,
    SpawnFailed,
}

pub struct RunResult {
    pub ok: bool,
    pub out: String,
    pub stderr: String,
    pub end: RunEnd,
}

impl RunResult {
    /// One-line diagnostic for failure logs: how the run ended, plus a stderr tail.
    pub fn failure_detail(&self) -> String {
        let how = match self.end {
            RunEnd::Exited(Some(code)) => format!("exit {code}"),
            RunEnd::Exited(None) => "killed by signal".to_string(),
            RunEnd::TimedOut => "timed out".to_string(),
            RunEnd::SpawnFailed => "spawn failed".to_string(),
        };
        let err: String = self.stderr.trim().chars().take(200).collect();
        if err.is_empty() {
            how
        } else {
            format!("{how}: {err}")
        }
    }
}

/// PATH that works no matter who spawned plant. The launchd KeepAlive agent hands us
/// a bare /usr/bin:/bin PATH with no herdr/zstd/cnb — augment with plant's own dir and
/// the usual user bin dirs (missing ones are harmless).
pub fn augmented_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut parts: Vec<String> = vec![];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            parts.push(dir.display().to_string());
        }
    }
    for d in [".nix-profile/bin", ".local/bin", ".bun/bin"] {
        parts.push(format!("{home}/{d}"));
    }
    parts.push("/opt/homebrew/bin".into());
    parts.push("/usr/local/bin".into());
    parts.push(std::env::var("PATH").unwrap_or_default());
    parts.join(":")
}

pub async fn run(cmd: &[&str], timeout: Duration) -> RunResult {
    let fut = tokio::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .env("PATH", augmented_path())
        .output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => RunResult {
            ok: o.status.success(),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            end: RunEnd::Exited(o.status.code()),
        },
        Ok(Err(e)) => RunResult {
            ok: false,
            out: String::new(),
            stderr: e.to_string(),
            end: RunEnd::SpawnFailed,
        },
        Err(_) => RunResult {
            ok: false,
            out: String::new(),
            stderr: String::new(),
            end: RunEnd::TimedOut,
        },
    }
}

pub async fn run30(cmd: &[&str]) -> RunResult {
    run(cmd, Duration::from_secs(30)).await
}

fn which(bin: &str) -> bool {
    augmented_path()
        .split(':')
        .any(|dir| Path::new(dir).join(bin).is_file())
}

/// sid -> latest `processed_at` (epoch secs; 0 when the entry has none) for `learner`.
/// Entries without a `learner` key are the legacy Claude pass.
fn ledger_latest(vault: &Path, learner: &str) -> HashMap<String, u64> {
    let mut processed = HashMap::new();
    let Ok(root) = vaultr::validate::content_root(vault) else {
        return processed; // rootless sessions path => nothing ledgered; non-fatal
    };
    if let Ok(text) = std::fs::read_to_string(vaultr::validate::ledger_path(&root)) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let recorded = v.get("learner").and_then(|s| s.as_str());
                if recorded == Some(learner) || (recorded.is_none() && learner == "claude") {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        let ts = v
                            .get("processed_at")
                            .and_then(|s| s.as_str())
                            .and_then(iso_to_epoch)
                            .unwrap_or(0);
                        let e = processed.entry(sid.to_string()).or_insert(0);
                        *e = (*e).max(ts);
                    }
                }
            }
        }
    }
    processed
}

/// "2026-07-14T15:08:26Z" (fractional seconds tolerated) -> epoch secs.
fn iso_to_epoch(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days-from-civil (Howard Hinnant): valid for all post-1970 dates we ledger
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
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

    fn learned_current(&self, latest: &HashMap<String, u64>) -> bool {
        let Some(&timestamp) = latest.get(&self.sid) else {
            return false;
        };
        if self.selected != GenerationKind::Raw {
            return true;
        }
        let previous = self
            .inventory
            .detached
            .as_ref()
            .map(|generation| generation.path.as_path())
            .or(self.inventory.sealed.as_deref());
        match previous
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        {
            None => true,
            Some(boundary) => timestamp > boundary.as_secs(),
        }
    }

    fn idle_secs(&self) -> Option<u64> {
        std::fs::metadata(self.path())
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map(|duration| duration.as_secs())
    }

    fn idle_for(&self, idle: Duration) -> bool {
        self.idle_secs()
            .is_some_and(|seconds| seconds >= idle.as_secs())
    }

    fn substantive(&self) -> bool {
        if self.selected != GenerationKind::Raw {
            return true;
        }
        let size = std::fs::metadata(self.path())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        size > 20_480
            || std::fs::read_to_string(self.path())
                .map(|text| text.trim_end().lines().count() > 5)
                .unwrap_or(false)
    }

    fn ready_to_seal(
        &self,
        claude: &HashMap<String, u64>,
        codex: &HashMap<String, u64>,
        jobs: &HashSet<String>,
        idle: Duration,
    ) -> bool {
        self.selected == GenerationKind::Detached
            || ((self.learned_current(claude) && self.learned_current(codex)
                || jobs.contains(&self.sid))
                && self.idle_for(idle))
    }
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn inflight_path(vault: &Path, learner: &str) -> Option<PathBuf> {
    let root = vaultr::validate::content_root(vault).ok()?;
    Some(
        root.join("learnings")
            .join(format!(".inflight-{learner}.json")),
    )
}

/// Sessions a still-running learn pass has claimed for `learner`, if the lease hasn't
/// expired. The ledger is only written when a pass *completes*, so without this the next
/// scheduler tick re-selects the same not-yet-ledgered batch and double-learns it (the
/// duplicate-prompt bug). ponytail: one file per learner, expiry self-heals a crashed or
/// timed-out pass — no explicit release path to leak.
fn inflight_sessions(vault: &Path, learner: &str) -> HashSet<String> {
    let Some(path) = inflight_path(vault, learner) else {
        return HashSet::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return HashSet::new();
    };
    if epoch_now() >= v.get("expires_at").and_then(|e| e.as_u64()).unwrap_or(0) {
        return HashSet::new(); // expired lease: the pass is long gone, stop excluding
    }
    v.get("sids")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Claim `sids` as in-flight for `learner` until `expires_at` (epoch secs). Overwrites any
/// prior lease for this learner. The learn job calls this at dispatch, before typing the
/// prompt, so a slow pass can't be re-dispatched underneath it.
pub fn claim_inflight(vault: &Path, learner: &str, sids: &[String], expires_at: u64) {
    let body = serde_json::json!({ "sids": sids, "expires_at": expires_at });
    let Some(path) = inflight_path(vault, learner) else {
        return; // rootless sessions path: skip the lease rather than write astray
    };
    let _ = std::fs::write(path, body.to_string());
}

/// Validated capture inventories under YYYY/MM/DD/<id>. The shared walker rejects
/// symlinked numeric levels and the inventory validates every generation before selection.
fn session_generations(
    vault: &Path,
    select: fn(String, vaultr::vault::CaptureGenerations) -> Option<SessionGeneration>,
) -> Result<Vec<SessionGeneration>, String> {
    let mut out = vec![];
    for (sid, sess) in vaultr::vault::walk_session_dirs(vault).map_err(|e| e.to_string())? {
        let inventory =
            vaultr::vault::CaptureGenerations::load(&sess).map_err(|e| e.to_string())?;
        if let Some(generation) = select(sid, inventory) {
            out.push(generation);
        }
    }
    Ok(out)
}

fn current_generations(vault: &Path) -> Result<Vec<SessionGeneration>, String> {
    session_generations(vault, SessionGeneration::current)
}

fn pending_generations(vault: &Path) -> Result<Vec<SessionGeneration>, String> {
    session_generations(vault, SessionGeneration::pending_seal)
}

/// One raw Session Capture the cultivation pipeline should have sealed by now.
pub struct StuckCapture {
    pub sid: String,
    /// seal-blocked | half-learned:<missing learner> | unlearned | sub-threshold
    pub state: String,
    pub idle_secs: u64,
}

/// Classify raw captures idle >= `age` by learn-ledger state. Read-only; the watchdog
/// job and `plant sessions stuck` both call this. sub-threshold captures can never seal
/// under current rules (learn skips them, sealing needs both ledgers) — reported so the
/// gap stays visible, but callers treat them as informational, not actionable.
pub fn stuck_captures(vault: &Path, age: Duration) -> Result<Vec<StuckCapture>, String> {
    let claude = ledger_latest(vault, "claude");
    let codex = ledger_latest(vault, "codex");
    let mut out = vec![];
    let jobs = job_sids();
    for generation in pending_generations(vault)? {
        let Some(idle) = generation.idle_secs() else {
            continue;
        };
        if idle < age.as_secs() {
            continue;
        }
        let state = if jobs.contains(&generation.sid) {
            "job-capture".to_string() // plant's own agent pane; informational like sub-threshold
        } else {
            match (
                generation.learned_current(&claude),
                generation.learned_current(&codex),
            ) {
                (true, true) => "seal-blocked".to_string(),
                (true, false) => "half-learned:codex".to_string(),
                (false, true) => "half-learned:claude".to_string(),
                (false, false) if generation.substantive() => "unlearned".to_string(),
                (false, false) => "sub-threshold".to_string(),
            }
        };
        out.push(StuckCapture {
            sid: generation.sid,
            state,
            idle_secs: idle,
        });
    }
    Ok(out)
}

/// Sids of agent panes plant itself launched for scheduled jobs. Learn passes must not
/// dispatch on these self-captures — a learn pane's own capture fed back into the next
/// learn run cost a full agent run per learner just to be ledgered "skipped".
/// `plant agent run` registers the --session-id it hands the claude CLI before launch;
/// for codex (server-assigned conversation ids) `run_agent` reads the id herdr reports
/// for the pane once the run finishes and registers it, so both harnesses are excluded
/// before dispatch — no content-heuristic skip in the learn skill.
pub fn job_sids_path() -> PathBuf {
    crate::jobs::state_dir().join("job-sids.txt")
}

pub fn job_sids() -> HashSet<String> {
    job_sids_at(&job_sids_path())
}

fn job_sids_at(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn register_job_sid(sid: &str) {
    let path = job_sids_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{sid}");
    }
}

pub fn eligible_sessions(
    vault: &Path,
    idle: Duration,
    max: usize,
    learner: &str,
) -> Result<Vec<String>, String> {
    let processed = ledger_latest(vault, learner);
    let inflight = inflight_sessions(vault, learner);
    let jobs = job_sids();
    let mut out = vec![];
    for generation in current_generations(vault)? {
        if jobs.contains(&generation.sid)
            || generation.learned_current(&processed)
            || inflight.contains(&generation.sid)
            || !generation.idle_for(idle)
        {
            continue;
        }
        if generation.substantive() {
            if let Some(dir) = generation.path().parent() {
                out.push(dir.display().to_string());
            }
        }
    }
    out.truncate(max);
    Ok(out)
}

/// Diagnostics for the `sessions eligible` subcommand's stderr — stdout must stay
/// clean (it is substituted into an agent prompt by the learn job).
pub fn eligibility_stats(vault: &Path, learner: &str) -> Result<(usize, usize), String> {
    Ok((
        current_generations(vault)?.len(),
        ledger_latest(vault, learner).len(),
    ))
}

/// Unambiguous secret formats — low false-positive prefixes only. Deliberately NOT a
/// generic-entropy heuristic: gitleaks' generic-api-key rule only ever flagged session
/// UUIDs here (all false positives) and was the 120s-cap timeout on multi-GB files.
/// Auth/x-api-key headers never reach the vault (capture.rs allowed_headers allowlist),
/// so this defends message *bodies*. Compiled once per scrub, not per line.
fn secret_regexes() -> Vec<regex::Regex> {
    [
        r"sk-ant-[A-Za-z0-9_-]{20,}", // Anthropic
        // ponytail: no bare `sk-[alnum]{20,}` — it over-matches base64 (the exact entropy
        // false-positive we dropped gitleaks for). Add sk-proj-/sk-[alnum]{48} if OpenAI
        // keys ever show up in bodies (we capture Anthropic traffic, so they don't yet).
        r"(?:AKIA|ASIA)[0-9A-Z]{16}",    // AWS access key
        r"gh[posru]_[A-Za-z0-9]{36,}",   // GitHub token
        r"xox[baprs]-[A-Za-z0-9-]{10,}", // Slack token
        r"https://hooks\.slack\.com/services/[A-Za-z0-9/]+", // Slack webhook
        r"AIza[0-9A-Za-z_-]{35}",        // Google API key
        r"ya29\.[0-9A-Za-z_-]{20,}",     // Google OAuth
        r"(?s)-----BEGIN[A-Z ]*PRIVATE KEY-----.*?-----END[A-Z ]*PRIVATE KEY-----", // PEM
    ]
    .iter()
    .filter_map(|p| regex::Regex::new(p).ok())
    .collect()
}

/// Redact one line against literal denylist needles + secret regexes. Pure — the streaming
/// loop and the self-test both call it. Returns the rewritten line and the redaction count.
fn redact_line(
    mut line: String,
    needles: &HashSet<String>,
    patterns: &[regex::Regex],
) -> (String, usize) {
    let mut hits = 0;
    for needle in needles {
        let c = line.matches(needle.as_str()).count();
        if c > 0 {
            hits += c;
            line = line.replace(needle.as_str(), "[REDACTED]");
        }
    }
    for re in patterns {
        if re.is_match(&line) {
            hits += re.find_iter(&line).count();
            line = re.replace_all(&line, "[REDACTED]").into_owned();
        }
    }
    (line, hits)
}

struct AnchoredDirectory {
    path: PathBuf,
    file: File,
}

impl AnchoredDirectory {
    fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| format!("open session directory {}: {error}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    fn name(&self, name: &str) -> Result<CString, String> {
        if name.is_empty()
            || Path::new(name).file_name().and_then(|part| part.to_str()) != Some(name)
        {
            return Err(format!(
                "invalid session entry name under {}",
                self.path.display()
            ));
        }
        CString::new(name)
            .map_err(|_| format!("invalid session entry name under {}", self.path.display()))
    }

    fn open_optional(&self, name: &str, write: bool) -> Result<Option<File>, String> {
        let name_c = self.name(name)?;
        let flags = if write { libc::O_RDWR } else { libc::O_RDONLY }
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK;
        // SAFETY: `name_c` is NUL-terminated, the retained directory fd is
        // valid, and a successful descriptor is transferred into `File`.
        let descriptor = unsafe { libc::openat(self.file.as_raw_fd(), name_c.as_ptr(), flags, 0) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(format!(
                "open session entry {}: {error}",
                self.path.join(name).display()
            ));
        }
        // SAFETY: `openat` returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(descriptor) };
        if !file
            .metadata()
            .map_err(|error| {
                format!(
                    "inspect session entry {}: {error}",
                    self.path.join(name).display()
                )
            })?
            .is_file()
        {
            return Err(format!(
                "session entry is not a regular file at {}",
                self.path.join(name).display()
            ));
        }
        Ok(Some(file))
    }

    fn open_required(&self, name: &str, write: bool) -> Result<File, String> {
        self.open_optional(name, write)?.ok_or_else(|| {
            format!(
                "missing session entry at {}",
                self.path.join(name).display()
            )
        })
    }

    fn create_temp(&self, base: &str, purpose: &str) -> Result<(String, File), String> {
        let name = format!(".{base}.{purpose}-{}", uuid::Uuid::new_v4());
        let name_c = self.name(&name)?;
        let flags =
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        // SAFETY: `name_c` is NUL-terminated, the retained directory fd is
        // valid, and a successful descriptor is transferred into `File`.
        let descriptor =
            unsafe { libc::openat(self.file.as_raw_fd(), name_c.as_ptr(), flags, 0o600) };
        if descriptor < 0 {
            return Err(format!(
                "create session temp {}: {}",
                self.path.join(&name).display(),
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `openat` returned a new owned descriptor.
        Ok((name, unsafe { File::from_raw_fd(descriptor) }))
    }

    fn entry_names(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.path)
            .map_err(|error| format!("read session directory {}: {error}", self.path.display()))?
        {
            let entry = entry.map_err(|error| {
                format!("read session entry under {}: {error}", self.path.display())
            })?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        let current = Self::open(&self.path)?;
        if !Self::same_file(&self.file, &current.file)? {
            return Err(format!(
                "session directory changed during inventory at {}",
                self.path.display()
            ));
        }
        Ok(names)
    }

    fn cleanup_temps(
        &self,
        base: &str,
        purposes: &[&str],
        legacy_names: &[&str],
    ) -> Result<(), String> {
        let mut owned_names = Vec::new();
        for name in self.entry_names()? {
            let mut prefixed = false;
            let exact_uuid = purposes.iter().any(|purpose| {
                let prefix = format!(".{base}.{purpose}-");
                name.strip_prefix(&prefix).is_some_and(|suffix| {
                    prefixed = true;
                    uuid::Uuid::parse_str(suffix).is_ok_and(|id| {
                        id.get_version_num() == 4 && id.hyphenated().to_string() == suffix
                    })
                })
            });
            let exact_legacy = legacy_names.iter().any(|legacy| name == *legacy);
            let legacy_near_miss = legacy_names.iter().any(|legacy| {
                legacy
                    .strip_suffix("tmp")
                    .is_some_and(|prefix| name != *legacy && name.starts_with(prefix))
            });
            if (prefixed && !exact_uuid) || legacy_near_miss {
                return Err(format!(
                    "unrecognized session temp evidence at {}",
                    self.path.join(name).display()
                ));
            }
            if !exact_uuid && !exact_legacy {
                continue;
            }
            owned_names.push(name);
        }
        // Validate and retain every exact entry before removing any. A symlink
        // or non-regular legacy entry therefore leaves all evidence untouched.
        let mut owned = Vec::with_capacity(owned_names.len());
        for name in owned_names {
            let file = self.open_required(&name, false)?;
            owned.push((name, file));
        }
        for (name, file) in owned {
            self.unlink_if_same(&name, &file)?;
        }
        Ok(())
    }

    fn same_file(left: &File, right: &File) -> Result<bool, String> {
        let left = left
            .metadata()
            .map_err(|error| format!("inspect retained session entry: {error}"))?;
        let right = right
            .metadata()
            .map_err(|error| format!("inspect current session entry: {error}"))?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }

    fn entry_matches(&self, name: &str, expected: &File) -> Result<bool, String> {
        let Some(current) = self.open_optional(name, false)? else {
            return Ok(false);
        };
        Self::same_file(&current, expected)
    }

    fn sync(&self) -> Result<(), String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync session directory {}: {error}", self.path.display()))
    }

    /// Cooperative cross-process ownership for one session's maintenance
    /// transaction. The retained directory fd keeps the flock until drop.
    fn lock_exclusive(&self) -> Result<(), String> {
        // SAFETY: the retained directory descriptor is valid for this object's
        // lifetime; `flock` does not take ownership of it.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock session directory {}: {}",
                self.path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Under the caller-held cooperative lock, rename only the retained inode,
    /// then make that directory entry durable before returning.
    fn replace_entry(
        &self,
        from: &str,
        to: &str,
        source: &File,
        expected_destination: Option<&File>,
    ) -> Result<File, String> {
        source.sync_all().map_err(|error| {
            format!(
                "sync session entry before rename {}: {error}",
                self.path.join(from).display()
            )
        })?;
        if !self.entry_matches(from, source)? {
            return Err(format!(
                "session entry changed before rename at {}",
                self.path.join(from).display()
            ));
        }
        match (self.open_optional(to, false)?, expected_destination) {
            (Some(current), Some(expected)) if Self::same_file(&current, expected)? => {}
            (None, None) => {}
            _ => {
                return Err(format!(
                    "session destination changed before rename at {}",
                    self.path.join(to).display()
                ));
            }
        }
        let from_c = self.name(from)?;
        let to_c = self.name(to)?;
        // SAFETY: both names are NUL-terminated and resolved only relative to
        // the retained directory descriptor.
        let status = unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                from_c.as_ptr(),
                self.file.as_raw_fd(),
                to_c.as_ptr(),
            )
        };
        if status != 0 {
            return Err(format!(
                "rename session entry {}: {}",
                self.path.join(from).display(),
                std::io::Error::last_os_error()
            ));
        }
        let renamed = self.open_required(to, false)?;
        if !Self::same_file(&renamed, source)? {
            return Err(format!(
                "session entry changed during rename at {}",
                self.path.join(to).display()
            ));
        }
        self.sync()?;
        source
            .try_clone()
            .map_err(|error| format!("retain renamed session entry: {error}"))
    }

    /// Under the caller-held cooperative lock, remove only the retained inode
    /// and make the removal durable before success.
    fn unlink_if_same(&self, name: &str, expected: &File) -> Result<(), String> {
        if !self.entry_matches(name, expected)? {
            return Err(format!(
                "session entry changed before removal at {}",
                self.path.join(name).display()
            ));
        }
        let name_c = self.name(name)?;
        // SAFETY: `name_c` is NUL-terminated and resolved only relative to the
        // retained directory descriptor.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
            return Err(format!(
                "remove session entry {}: {}",
                self.path.join(name).display(),
                std::io::Error::last_os_error()
            ));
        }
        self.sync()
    }
}

fn clone_at_start(file: &File) -> Result<File, String> {
    let mut clone = file
        .try_clone()
        .map_err(|error| format!("clone retained session entry: {error}"))?;
    clone
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek retained session entry: {error}"))?;
    Ok(clone)
}

fn hash_file(file: &File) -> Result<String, String> {
    vaultr::vault::sha256_reader(clone_at_start(file)?).map_err(|error| error.to_string())
}

fn print_scrub(path: &Path, hits: usize) {
    if hits == 0 {
        return;
    }
    let path_s = path.display().to_string();
    let rel = path_s.split("/sessions/").nth(1).unwrap_or(&path_s);
    println!("[scrub] {rel}: {hits} redaction(s)");
}

fn scrub_entry(directory: &AnchoredDirectory, name: &str) -> Result<(File, usize), String> {
    use std::io::{BufRead, BufReader, BufWriter};

    let legacy_temps: &[&str] = if name == "turns.jsonl" {
        &["turns.scrub-tmp"]
    } else {
        &[]
    };
    directory.cleanup_temps(name, &["scrub"], legacy_temps)?;
    let source = directory.open_required(name, true)?;
    let mut needles: HashSet<String> = HashSet::new();
    let denylist = format!(
        "{}/.config/wireproxy/scrub-denylist.txt",
        std::env::var("HOME").unwrap_or_default()
    );
    if let Ok(contents) = std::fs::read_to_string(&denylist) {
        for line in contents.lines() {
            let value = line.trim();
            if value.len() >= 6 {
                let escaped = serde_json::to_string(value).unwrap_or_default();
                needles.insert(value.to_string());
                if escaped.len() >= 2 {
                    needles.insert(escaped[1..escaped.len() - 1].to_string());
                }
            }
        }
    }
    let patterns = secret_regexes();
    let (temporary_name, temporary) = directory.create_temp(name, "scrub")?;
    let scrubbed = (|| -> Result<usize, String> {
        let reader = BufReader::new(clone_at_start(&source)?);
        let mut writer = BufWriter::new(
            temporary
                .try_clone()
                .map_err(|error| format!("clone scrub temp: {error}"))?,
        );
        let mut hits = 0;
        for line in reader.lines() {
            let (line, count) = redact_line(
                line.map_err(|error| format!("read session capture: {error}"))?,
                &needles,
                &patterns,
            );
            hits += count;
            writeln!(writer, "{line}")
                .map_err(|error| format!("write scrubbed session capture: {error}"))?;
        }
        writer
            .flush()
            .map_err(|error| format!("flush scrubbed session capture: {error}"))?;
        temporary
            .sync_all()
            .map_err(|error| format!("sync scrubbed session capture: {error}"))?;
        Ok(hits)
    })();
    let hits = match scrubbed {
        Ok(hits) => hits,
        Err(error) => {
            let _ = directory.unlink_if_same(&temporary_name, &temporary);
            return Err(error);
        }
    };
    if hits == 0 {
        directory.unlink_if_same(&temporary_name, &temporary)?;
        return Ok((source, 0));
    }
    match directory.replace_entry(&temporary_name, name, &temporary, Some(&source)) {
        Ok(scrubbed) => Ok((scrubbed, hits)),
        Err(error) => {
            let _ = directory.unlink_if_same(&temporary_name, &temporary);
            Err(error)
        }
    }
}

/// Redact known-secret patterns + a literal denylist in place. false => do not compress/push.
/// Rust-native (regex): no subprocess, no timeout, constant memory. Streaming line-by-line —
/// turns.jsonl files reach GBs, and whole-file reads were the historic multi-GB RSS spike
/// (and the Bun-era jetsam death loop).
pub async fn scrub(path: &Path) -> bool {
    let Some(directory_path) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Ok(directory) = AnchoredDirectory::open(directory_path) else {
        return false;
    };
    if directory.lock_exclusive().is_err() {
        return false;
    }
    let Ok((_, hits)) = scrub_entry(&directory, name) else {
        return false;
    };
    print_scrub(path, hits);
    true
}

/// Detach one mutable sidecar generation under the caller-held session lock.
/// A retained detached file always wins over a newer raw sibling.
fn detach_sidecar(
    raw: &Path,
    dest: &Path,
) -> Result<Option<vaultr::vault::DetachedGeneration>, String> {
    let directory_path = raw
        .parent()
        .ok_or_else(|| format!("sidecar has no directory at {}", raw.display()))?;
    if dest.parent() != Some(directory_path) {
        return Err(format!(
            "sidecar destination leaves session directory at {}",
            dest.display()
        ));
    }
    let directory = AnchoredDirectory::open(directory_path)?;
    directory.lock_exclusive()?;
    let raw_name = raw
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid sidecar name at {}", raw.display()))?;
    let dest_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid sealed sidecar name at {}", dest.display()))?;
    let prefix = format!("{raw_name}.sealing-");
    let mut detached = None;
    for name in directory.entry_names()? {
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let path = directory_path.join(&name);
        let (base_len, digest) = suffix
            .split_once('-')
            .ok_or_else(|| format!("invalid detached sidecar at {}", path.display()))?;
        let base_len = base_len
            .parse::<u64>()
            .map_err(|_| format!("invalid detached sidecar at {}", path.display()))?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("invalid detached sidecar at {}", path.display()));
        }
        let digest = digest.to_ascii_lowercase();
        let file = directory.open_required(&name, false)?;
        if hash_file(&file)? != digest {
            return Err(format!(
                "detached sidecar digest mismatch at {}",
                path.display()
            ));
        }
        if detached
            .replace(vaultr::vault::DetachedGeneration {
                path,
                base_len,
                digest,
            })
            .is_some()
        {
            return Err(format!(
                "multiple detached sidecar generations under {}",
                directory_path.display()
            ));
        }
    }
    if detached.is_some() {
        return Ok(detached);
    }

    let Some(source) = directory.open_optional(raw_name, true)? else {
        return Ok(None);
    };
    let base_len = directory
        .open_optional(dest_name, false)?
        .map(|file| file.metadata())
        .transpose()
        .map_err(|error| format!("inspect sealed sidecar {}: {error}", dest.display()))?
        .map_or(0, |metadata| metadata.len());
    let digest = hash_file(&source)?;
    let detached_name = format!("{raw_name}.sealing-{base_len}-{digest}");
    directory.replace_entry(raw_name, &detached_name, &source, None)?;
    let path = directory_path.join(detached_name);
    Ok(Some(vaultr::vault::DetachedGeneration {
        path,
        base_len,
        digest,
    }))
}

pub(crate) fn detach_capture_generation(
    directory_path: &Path,
) -> Result<vaultr::vault::DetachedGeneration, String> {
    let directory = AnchoredDirectory::open(directory_path)?;
    directory.lock_exclusive()?;
    let (source, hits) = scrub_entry(&directory, "turns.jsonl")?;
    print_scrub(&directory_path.join("turns.jsonl"), hits);
    let base_len = directory
        .open_optional("turns.jsonl.zst", false)?
        .map(|file| file.metadata())
        .transpose()
        .map_err(|error| {
            format!(
                "inspect sealed generation under {}: {error}",
                directory_path.display()
            )
        })?
        .map_or(0, |metadata| metadata.len());
    let digest = hash_file(&source)?;
    let detached_name = format!("turns.jsonl.sealing-{base_len}-{digest}");
    directory.replace_entry("turns.jsonl", &detached_name, &source, None)?;
    Ok(vaultr::vault::DetachedGeneration {
        path: directory_path.join(detached_name),
        base_len,
        digest,
    })
}

async fn wait_for_compressor(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, String> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(format!("wait for zstd: {error}"))
        }
        Err(_) => {
            let kill_error = child.start_kill().err();
            let reap = child.wait().await;
            match (kill_error, reap) {
                (_, Ok(_)) => Err("zstd timed out; killed and reaped".into()),
                (Some(kill), Err(wait)) => Err(format!(
                    "zstd timed out; kill failed: {kill}; reap failed: {wait}"
                )),
                (None, Err(wait)) => Err(format!("zstd timed out; reap failed: {wait}")),
            }
        }
    }
}

async fn compress_frame_with_timeout(
    source: &File,
    frame: &File,
    timeout: Duration,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let source_len = source
        .metadata()
        .map_err(|error| format!("inspect detached generation: {error}"))?
        .len();
    frame
        .set_len(0)
        .map_err(|error| format!("truncate compression temp: {error}"))?;
    let input = clone_at_start(source)?;
    let output = clone_at_start(frame)?;
    let stream_size = format!("--stream-size={source_len}");
    let mut command = tokio::process::Command::new("zstd");
    command
        .args(["-19", "-T0", "-q", "-c", &stream_size])
        .env("PATH", augmented_path())
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn zstd: {error}"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture zstd stderr unavailable".to_string())?;
    let (status, stderr) = tokio::join!(wait_for_compressor(&mut child, timeout), async move {
        let mut bytes = Vec::new();
        stderr
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
            .map_err(|error| format!("read zstd stderr: {error}"))
    });
    let status = status?;
    let stderr = stderr?;
    if !status.success() {
        let stderr: String = String::from_utf8_lossy(&stderr)
            .trim()
            .chars()
            .take(200)
            .collect();
        return Err(if stderr.is_empty() {
            format!("zstd exit {status}")
        } else {
            format!("zstd exit {status}: {stderr}")
        });
    }
    frame
        .sync_all()
        .map_err(|error| format!("sync compression temp: {error}"))
}

async fn compress_frame(source: &File, frame: &File) -> Result<(), String> {
    compress_frame_with_timeout(source, frame, Duration::from_secs(600)).await
}

#[derive(Clone, Copy)]
enum FrameCompressor {
    Zstd,
    #[cfg(test)]
    CorruptSuccess,
}

async fn write_frame(
    compressor: FrameCompressor,
    source: &File,
    frame: &File,
) -> Result<(), String> {
    match compressor {
        FrameCompressor::Zstd => compress_frame(source, frame).await,
        #[cfg(test)]
        FrameCompressor::CorruptSuccess => {
            let mut output = clone_at_start(frame)?;
            output
                .write_all(b"not a zstd frame")
                .and_then(|_| output.flush())
                .map_err(|error| format!("write corrupt-success fixture: {error}"))?;
            frame
                .sync_all()
                .map_err(|error| format!("sync corrupt-success fixture: {error}"))
        }
    }
}

/// Commit one immutable detached generation exactly once. The filename records
/// the sealed destination length at detach time; cleanup requires the canonical
/// decoded suffix digest proof, independent of compressed frame representation.
async fn seal_generation(
    generation: &vaultr::vault::DetachedGeneration,
    dest: &Path,
) -> Result<PathBuf, String> {
    seal_generation_with(generation, dest, FrameCompressor::Zstd).await
}

async fn seal_generation_with(
    generation: &vaultr::vault::DetachedGeneration,
    dest: &Path,
    compressor: FrameCompressor,
) -> Result<PathBuf, String> {
    let directory_path = generation.path.parent().ok_or_else(|| {
        format!(
            "detached generation has no directory at {}",
            generation.path.display()
        )
    })?;
    if dest.parent() != Some(directory_path) {
        return Err(format!(
            "sealed destination leaves session directory at {}",
            dest.display()
        ));
    }
    let source_name = generation
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "invalid detached generation name at {}",
                generation.path.display()
            )
        })?;
    let dest_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid sealed generation name at {}", dest.display()))?;
    let directory = AnchoredDirectory::open(directory_path)?;
    directory.lock_exclusive()?;
    let source = directory.open_required(source_name, true)?;
    source.sync_all().map_err(|error| {
        format!(
            "sync detached generation {}: {error}",
            generation.path.display()
        )
    })?;
    if hash_file(&source)? != generation.digest {
        return Err(format!(
            "detached generation digest mismatch at {}",
            generation.path.display()
        ));
    }
    let raw_mtime = source
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|error| {
            format!(
                "inspect detached generation {}: {error}",
                generation.path.display()
            )
        })?;
    // These four deterministic sealing names, plus turns.scrub-tmp above, are
    // the complete temp vocabulary of the immediately preceding release. They
    // are migration evidence, not a new generation classifier.
    let legacy_temps: &[&str] = match dest_name {
        "turns.jsonl.zst" => &["turns.jsonl.frame-tmp", "turns.jsonl.zst-tmp"],
        "herdr.jsonl.zst" => &["herdr.jsonl.frame-tmp", "herdr.jsonl.zst-tmp"],
        _ => &[],
    };
    directory.cleanup_temps(dest_name, &["frame", "merged"], legacy_temps)?;
    let destination = directory.open_optional(dest_name, false)?;
    let dest_len = destination
        .as_ref()
        .map(|file| file.metadata())
        .transpose()
        .map_err(|error| format!("inspect sealed destination {}: {error}", dest.display()))?
        .map_or(0, |metadata| metadata.len());
    let committed = if dest_len > generation.base_len {
        destination.expect("positive destination length")
    } else if dest_len == generation.base_len {
        let (frame_name, frame) = directory.create_temp(dest_name, "frame")?;
        if let Err(error) = write_frame(compressor, &source, &frame).await {
            let _ = directory.unlink_if_same(&frame_name, &frame);
            return Err(error);
        }
        let (merged_name, merged) = match directory.create_temp(dest_name, "merged") {
            Ok(merged) => merged,
            Err(error) => {
                let _ = directory.unlink_if_same(&frame_name, &frame);
                return Err(error);
            }
        };
        let assembled = (|| -> Result<(), String> {
            let mut output = clone_at_start(&merged)?;
            if let Some(destination) = &destination {
                std::io::copy(&mut clone_at_start(destination)?, &mut output)
                    .map_err(|error| format!("copy sealed generation: {error}"))?;
            }
            std::io::copy(&mut clone_at_start(&frame)?, &mut output)
                .map_err(|error| format!("copy compressed generation: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("flush merged generation: {error}"))?;
            merged
                .sync_all()
                .map_err(|error| format!("sync merged generation: {error}"))
        })();
        if let Err(error) = assembled {
            let _ = directory.unlink_if_same(&merged_name, &merged);
            let _ = directory.unlink_if_same(&frame_name, &frame);
            return Err(error);
        }
        let renamed =
            directory.replace_entry(&merged_name, dest_name, &merged, destination.as_ref());
        if renamed.is_err() {
            let _ = directory.unlink_if_same(&merged_name, &merged);
        }
        let frame_cleanup = directory.unlink_if_same(&frame_name, &frame);
        let committed = renamed?;
        frame_cleanup?;
        committed
    } else {
        return Err(format!(
            "sealed destination conflicts with detached generation at {}",
            dest.display()
        ));
    };
    let decoded_digest =
        vaultr::vault::decoded_zstd_suffix_digest(clone_at_start(&committed)?, generation.base_len)
            .map_err(|_| format!("sealed destination suffix is invalid at {}", dest.display()))?;
    if decoded_digest != generation.digest {
        return Err(format!(
            "sealed destination conflicts with detached generation at {}",
            dest.display()
        ));
    }
    committed
        .set_modified(raw_mtime)
        .map_err(|error| format!("set mtime {}: {error}", dest.display()))?;
    committed
        .sync_all()
        .map_err(|error| format!("sync sealed destination {}: {error}", dest.display()))?;
    if !directory.entry_matches(dest_name, &committed)? {
        return Err(format!(
            "sealed destination changed before detached cleanup at {}",
            dest.display()
        ));
    }
    directory.sync()?;
    directory.unlink_if_same(source_name, &source)?;
    Ok(dest.to_path_buf())
}

/// Sealed captures above this stay on disk but out of git: a 2.7GB .zst blob turned
/// the vault push into a multi-hour hang (2026-07-18, codex session with
/// zstd-encoded bodies the outer seal couldn't shrink).
/// ponytail: fixed 256MB; make it a Cfg key if a legitimate capture ever needs pushing.
const COMMIT_CAP: u64 = 256 * 1024 * 1024;

/// Append `sealed` to the vault repo's .gitignore so `git add -A sessions` skips it.
/// Idempotent; the capture data itself is never touched (append-only rule).
fn exclude_from_commit(vault: &Path, sealed: &Path, size: u64) {
    let Ok(root) = vaultr::validate::content_root(vault) else {
        return;
    };
    let Ok(rel) = sealed.strip_prefix(&root) else {
        return;
    };
    let line = rel.display().to_string();
    let gi = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gi)
    {
        let _ = writeln!(
            f,
            "# oversized seal (auto, {:.1}GB): kept on disk, excluded from git\n{line}",
            size as f64 / 1e9
        );
        println!(
            "[compress] {line}: {:.1}GB exceeds commit cap, gitignored",
            size as f64 / 1e9
        );
    }
}

pub async fn compress_sweep(vault: &Path, idle: Duration) -> Result<(), String> {
    if !which("zstd") {
        return Err("zstd not on PATH".into());
    }
    let claude = ledger_latest(vault, "claude");
    let codex = ledger_latest(vault, "codex");
    let jobs = job_sids();
    let root = crate::capture::canonical_root(vault);
    let mut sealed = 0u32;
    for selected in pending_generations(vault)? {
        if !selected.ready_to_seal(&claude, &codex, &jobs, idle) {
            continue;
        }
        let sid = &selected.sid;
        let Some(generation) =
            crate::capture::detach_generation(vault, sid, selected.path().parent().unwrap())
                .await?
        else {
            continue;
        };
        let before = std::fs::metadata(&generation.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let herdr = generation.path.with_file_name("herdr.jsonl");
        let herdr_dest = generation.path.with_file_name("herdr.jsonl.zst");
        let herdr_generation = {
            let lock = crate::capture::session_lock(&root, sid);
            let _guard = lock.lock().await;
            detach_sidecar(&herdr, &herdr_dest)?
        };
        if let Some(herdr_generation) = herdr_generation {
            if let Err(e) = seal_generation(&herdr_generation, &herdr_dest).await {
                return Err(format!("seal {sid} herdr.jsonl: {e}"));
            }
        }
        let capture_dest = generation.path.with_file_name("turns.jsonl.zst");
        match seal_generation(&generation, &capture_dest).await {
            Ok(sealed_path) => {
                sealed += 1;
                let after = std::fs::metadata(&sealed_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if after > COMMIT_CAP {
                    exclude_from_commit(vault, &sealed_path, after);
                }
                let path_s = sealed_path.display().to_string();
                let rel = path_s.split("/sessions/").nth(1).unwrap_or(&path_s);
                println!(
                    "[compress] {rel}: {:.1}MB -> {:.1}MB",
                    before as f64 / 1e6,
                    after as f64 / 1e6
                );
            }
            Err(e) => {
                return Err(format!("seal detached generation for {sid}: {e}"));
            }
        }
    }
    if sealed > 0 {
        // A bare relative sessions root has an empty parent — the repo is the cwd.
        // A rootless path (no parent at all) has no repo: skip the commit, keep sealing.
        let repo = match vaultr::validate::content_root(vault) {
            Ok(p) if p.as_os_str().is_empty() => ".".to_string(),
            Ok(p) => p.to_str().unwrap_or(".").to_string(),
            Err(_) => {
                println!("[compress] sealed {sealed}, commit skipped: sessions root has no parent");
                return Ok(());
            }
        };
        run30(&["git", "-C", &repo, "add", "-A", "sessions"]).await;
        let msg = format!("chore: seal {sealed} session(s) (scrubbed + zstd)");
        run30(&["git", "-C", &repo, "commit", "-m", &msg]).await;
        let push = run(&["git", "-C", &repo, "push"], Duration::from_secs(300)).await;
        println!(
            "[compress] sealed {sealed}, push {}",
            if push.ok {
                "ok"
            } else {
                "FAILED (next sweep retries)"
            }
        );
    } else {
        println!("[compress] nothing to seal");
    }
    Ok(())
}

/// Read the selected capture generation's envelope lines. Unreadable => empty,
/// never fatal; the validated generation kind determines decoding.
fn capture_lines(generation: &SessionGeneration) -> Vec<String> {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(generation.path()) else {
        return vec![];
    };
    let mut text = String::new();
    let ok = if generation.selected == GenerationKind::Sealed {
        zstd::Decoder::new(file)
            .and_then(|mut d| d.read_to_string(&mut text))
            .is_ok()
    } else {
        let mut f = file;
        f.read_to_string(&mut text).is_ok()
    };
    if !ok {
        return vec![];
    }
    text.lines().map(String::from).collect()
}

/// Capture completeness for one Session Capture, measured over Plant's observation
/// window (see ADR-0001). A resumed session's pre-window transcript history is
/// reported as `carryover`, never as loss.
pub struct Coverage {
    pub sid: String,
    /// Earliest captured `observed_at`, else meta `original_start`.
    pub window_start: String,
    pub resumed: bool,
    /// Distinct native assistant `requestId`s at or after the window start.
    pub in_window_native: usize,
    /// Distinct captured Envelope `request-id`s.
    pub captured: usize,
    /// Native `requestId`s predating the window (out-of-scope, not lost).
    pub carryover: usize,
    /// In-window native `requestId`s with no captured Envelope, sorted.
    pub missing: Vec<String>,
}

impl Coverage {
    /// In-window captured / in-window native, as a percentage. Empty window is 100%.
    pub fn pct(&self) -> f64 {
        if self.in_window_native == 0 {
            return 100.0;
        }
        let hit = self.in_window_native - self.missing.len();
        hit as f64 * 100.0 / self.in_window_native as f64
    }
}

/// Compute [`Coverage`] for a session id (or unambiguous prefix). Read-only over the
/// Session Capture and the harness transcript named in meta; mutates nothing.
pub fn coverage(vault: &Path, query: &str) -> Result<Coverage, String> {
    let session = vaultr::vault::resolve_id(vault, query).map_err(|e| e.to_string())?;
    let dir = vaultr::vault::session_dir(vault, &session).map_err(|e| e.to_string())?;
    let inventory = vaultr::vault::CaptureGenerations::load(&dir).map_err(|e| e.to_string())?;
    let generation = SessionGeneration::current(session.id.clone(), inventory)
        .ok_or_else(|| format!("no capture found for {}", session.id))?;

    // Captured side: distinct response request-ids, and the window start (min observed_at).
    let mut captured: HashSet<String> = HashSet::new();
    let mut window_start: Option<String> = None;
    for line in capture_lines(&generation) {
        let Ok(env) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(rid) = env
            .pointer("/response/headers/request-id")
            .and_then(|v| v.as_str())
        {
            captured.insert(rid.to_string());
        }
        if let Some(obs) = env.get("observed_at").and_then(|v| v.as_str()) {
            if window_start.as_deref().is_none_or(|w| obs < w) {
                window_start = Some(obs.to_string());
            }
        }
    }
    let window_start = window_start
        .or_else(|| session.meta.original_start.clone())
        .ok_or_else(|| format!("no envelopes and no original_start for {}", session.id))?;

    // Native side: assistant requestId -> earliest transcript timestamp.
    let transcript = session
        .meta
        .transcript_path
        .clone()
        .ok_or_else(|| format!("no transcript_path in meta for {}", session.id))?;
    let text = std::fs::read_to_string(&transcript)
        .map_err(|e| format!("read transcript {transcript}: {e}"))?;
    let mut first_seen: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(rid) = v.get("requestId").and_then(|r| r.as_str()) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        first_seen
            .entry(rid.to_string())
            .and_modify(|e| {
                if ts < *e {
                    *e = ts.clone();
                }
            })
            .or_insert(ts);
    }

    let mut in_window_native = 0usize;
    let mut carryover = 0usize;
    let mut missing = vec![];
    for (rid, ts) in &first_seen {
        if ts.as_str() >= window_start.as_str() {
            in_window_native += 1;
            if !captured.contains(rid) {
                missing.push(rid.clone());
            }
        } else {
            carryover += 1;
        }
    }
    missing.sort();

    Ok(Coverage {
        sid: session.id,
        window_start,
        resumed: session.meta.session_start_source.as_deref() == Some("resume"),
        in_window_native,
        captured: captured.len(),
        carryover,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal vault: meta index + dated session dir with turns.jsonl + a
    /// claude transcript. `envelopes` and `transcript` are raw file bodies.
    fn coverage_fixture(label: &str, resumed: bool, envelopes: &str, transcript: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("plant-cov-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sid = "cov00000-0000-4000-8000-000000000000";
        let start = "2026-07-17T19:00:00.000Z";
        let dir = root.join("2026/07/17").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("turns.jsonl"), envelopes).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, transcript).unwrap();
        std::fs::create_dir_all(root.join(".meta")).unwrap();
        let source = if resumed { "resume" } else { "wire" };
        std::fs::write(
            root.join(".meta").join(format!("{sid}.json")),
            format!(
                r#"{{"session_id":"{sid}","original_start":"{start}","session_start_source":"{source}","transcript_path":"{}"}}"#,
                transcript_path.display()
            ),
        )
        .unwrap();
        root
    }

    fn envelope(observed_at: &str, request_id: &str) -> String {
        format!(
            r#"{{"observed_at":"{observed_at}","response":{{"headers":{{"request-id":"{request_id}"}}}}}}"#
        )
    }
    fn assistant(ts: &str, request_id: &str) -> String {
        format!(r#"{{"type":"assistant","requestId":"{request_id}","timestamp":"{ts}"}}"#)
    }

    #[test]
    fn coverage_resume_carryover_is_not_loss() {
        // Window opens at the first envelope (19:18); pre-window transcript ids are carryover.
        let envelopes = format!("{}\n", envelope("2026-07-17T19:18:00.000Z", "req_A"));
        let transcript = format!(
            "{}\n{}\n{}\n",
            assistant("2026-07-17T18:23:00.000Z", "req_OLD1"),
            assistant("2026-07-17T18:24:00.000Z", "req_OLD2"),
            assistant("2026-07-17T19:18:00.000Z", "req_A"),
        );
        let root = coverage_fixture("carryover", true, &envelopes, &transcript);
        let c = coverage(&root, "cov00000").unwrap();
        assert!(c.resumed);
        assert_eq!(c.window_start, "2026-07-17T19:18:00.000Z");
        assert_eq!(c.carryover, 2, "pre-window ids are carryover");
        assert_eq!(c.in_window_native, 1);
        assert!(c.missing.is_empty(), "in-window fully captured");
        assert_eq!(c.pct(), 100.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn coverage_full_window_is_complete() {
        let envelopes = format!(
            "{}\n{}\n",
            envelope("2026-07-17T19:18:00.000Z", "req_A"),
            envelope("2026-07-17T19:19:00.000Z", "req_B"),
        );
        let transcript = format!(
            "{}\n{}\n",
            assistant("2026-07-17T19:18:00.000Z", "req_A"),
            assistant("2026-07-17T19:19:00.000Z", "req_B"),
        );
        let root = coverage_fixture("full", false, &envelopes, &transcript);
        let c = coverage(&root, "cov00000").unwrap();
        assert_eq!(c.in_window_native, 2);
        assert!(c.missing.is_empty());
        assert_eq!(c.pct(), 100.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn coverage_reports_genuine_in_window_gap() {
        // req_B is native in-window but never captured -> residual, coverage drops.
        let envelopes = format!("{}\n", envelope("2026-07-17T19:18:00.000Z", "req_A"));
        let transcript = format!(
            "{}\n{}\n",
            assistant("2026-07-17T19:18:00.000Z", "req_A"),
            assistant("2026-07-17T19:20:00.000Z", "req_B"),
        );
        let root = coverage_fixture("gap", false, &envelopes, &transcript);
        let c = coverage(&root, "cov00000").unwrap();
        assert_eq!(c.in_window_native, 2);
        assert_eq!(c.missing, vec!["req_B".to_string()]);
        assert_eq!(c.pct(), 50.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn eligibility_is_independent_per_learner() {
        let root = std::env::temp_dir().join(format!("plant-sweep-test-{}", std::process::id()));
        let sessions = root.join("sessions");
        let claude_id = "claude-processed";
        let codex_id = "codex-processed";
        let claude_dir = sessions.join("2026/07/15").join(claude_id);
        let codex_dir = sessions.join("2026/07/15").join(codex_id);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(claude_dir.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
        std::fs::write(codex_dir.join("turns.jsonl.zst"), "sealed").unwrap();
        std::fs::create_dir_all(root.join("learnings")).unwrap();
        std::fs::write(
            root.join("learnings/.ledger.jsonl"),
            format!(
                "{{\"session_id\":\"{claude_id}\"}}\n{{\"session_id\":\"{codex_id}\",\"learner\":\"codex\"}}\n"
            ),
        )
        .unwrap();

        let claude = eligible_sessions(&sessions, Duration::ZERO, 10, "claude").unwrap();
        let codex = eligible_sessions(&sessions, Duration::ZERO, 10, "codex").unwrap();
        assert!(claude.iter().any(|p| p.ends_with(codex_id)));
        assert!(!claude.iter().any(|p| p.ends_with(claude_id)));
        assert!(codex.iter().any(|p| p.ends_with(claude_id)));
        assert!(!codex.iter().any(|p| p.ends_with(codex_id)));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn walker_skips_non_date_dirs_but_finds_dated_sessions() {
        // The walker feeds the destructive sealing path: selection must be exact.
        let root = std::env::temp_dir().join(format!("plant-walker-test-{}", std::process::id()));
        let sessions = root.join("sessions");
        let sid = "abc123-real-session";
        let _ = std::fs::remove_dir_all(&root);
        // Normal dated session: must be found.
        let dated = sessions.join("2026/07/16").join(sid);
        std::fs::create_dir_all(&dated).unwrap();
        std::fs::write(dated.join("turns.jsonl"), "{}\n").unwrap();
        std::fs::write(dated.join("turns.jsonl.zst"), "prior seal").unwrap();
        // Non-date dirs at various levels: must never be descended into.
        for bogus in [
            ".meta/2026/07",
            "notes/07/16",
            "2026/backup/16",
            "2026/07/.tmp",
        ] {
            let d = sessions.join(bogus).join("phantom-session");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("turns.jsonl"), "{}\n").unwrap();
        }

        let raw = pending_generations(&sessions).unwrap();
        assert_eq!(raw.len(), 1, "only the dated session, got {raw:?}");
        assert_eq!(raw[0].sid, sid);
        assert_eq!(raw[0].selected, GenerationKind::Raw);
        assert_eq!(raw[0].path(), dated.join("turns.jsonl"));
        assert_eq!(
            raw[0].inventory.sealed.as_deref(),
            Some(dated.join("turns.jsonl.zst").as_path())
        );

        // Current selection retains the same validated inventory and chooses the seal
        // only after the raw generation is gone.
        std::fs::remove_file(dated.join("turns.jsonl")).unwrap();
        let all = current_generations(&sessions).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].selected, GenerationKind::Sealed);
        assert_eq!(all[0].path(), dated.join("turns.jsonl.zst"));
        assert!(pending_generations(&sessions).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stuck_classification_covers_every_ledger_state() {
        let root = std::env::temp_dir().join(format!("plant-stuck-test-{}", std::process::id()));
        let sessions = root.join("sessions");
        let day = sessions.join("2026/07/16");
        let _ = std::fs::remove_dir_all(&root);
        for (sid, body) in [
            ("both-ledgered", "{}\n".repeat(6)),  // seal-blocked
            ("claude-only", "{}\n".repeat(6)),    // half-learned:codex
            ("codex-only", "{}\n".repeat(6)),     // half-learned:claude
            ("nobody-big", "{}\n".repeat(6)),     // unlearned (>5 turns => substantive)
            ("nobody-small", "{}\n".to_string()), // sub-threshold
        ] {
            let d = day.join(sid);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("turns.jsonl"), body).unwrap();
        }
        // sealed capture: never reported regardless of ledger state
        let sealed = day.join("already-sealed");
        std::fs::create_dir_all(&sealed).unwrap();
        std::fs::write(sealed.join("turns.jsonl.zst"), "sealed").unwrap();
        std::fs::create_dir_all(root.join("learnings")).unwrap();
        std::fs::write(
            root.join("learnings/.ledger.jsonl"),
            concat!(
                "{\"session_id\":\"both-ledgered\"}\n",
                "{\"session_id\":\"both-ledgered\",\"learner\":\"codex\"}\n",
                "{\"session_id\":\"claude-only\",\"learner\":\"claude\"}\n",
                "{\"session_id\":\"codex-only\",\"learner\":\"codex\"}\n",
            ),
        )
        .unwrap();

        let stuck = stuck_captures(&sessions, Duration::ZERO).unwrap();
        let state = |sid: &str| {
            stuck
                .iter()
                .find(|s| s.sid == sid)
                .map(|s| s.state.clone())
                .unwrap_or_else(|| {
                    panic!(
                        "{sid} missing from {:?}",
                        stuck.iter().map(|s| &s.sid).collect::<Vec<_>>()
                    )
                })
        };
        assert_eq!(state("both-ledgered"), "seal-blocked");
        assert_eq!(state("claude-only"), "half-learned:codex");
        assert_eq!(state("codex-only"), "half-learned:claude");
        assert_eq!(state("nobody-big"), "unlearned");
        assert_eq!(state("nobody-small"), "sub-threshold");
        assert!(!stuck.iter().any(|s| s.sid == "already-sealed"));

        // freshly written files are idle ~0s: an age gate must exempt them all
        assert!(stuck_captures(&sessions, Duration::from_secs(3600))
            .unwrap()
            .is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inflight_lease_dedupes_dispatch_until_expiry() {
        // Reproduces the duplicate-prompt bug: an un-ledgered session is eligible, so a
        // second tick during a running pass would re-dispatch it. The lease must exclude it.
        let root = std::env::temp_dir().join(format!("plant-inflight-test-{}", std::process::id()));
        let sessions = root.join("sessions");
        let sid = "e768f4c4-inflight";
        let dir = sessions.join("2026/07/16").join(sid);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("turns.jsonl"), "{}\n".repeat(6)).unwrap(); // >5 turns => substantive
        std::fs::create_dir_all(root.join("learnings")).unwrap();

        // tick 1: not ledgered, not leased -> eligible (this is the dispatch that runs)
        let first = eligible_sessions(&sessions, Duration::ZERO, 10, "claude").unwrap();
        assert!(
            first.iter().any(|p| p.ends_with(sid)),
            "un-ledgered session should be eligible"
        );

        // that dispatch claims the batch in-flight for the pass's lifetime
        claim_inflight(&sessions, "claude", &[sid.to_string()], epoch_now() + 3600);

        // tick 2 while the pass is still running (ledger not yet written): must be excluded
        let during = eligible_sessions(&sessions, Duration::ZERO, 10, "claude").unwrap();
        assert!(
            !during.iter().any(|p| p.ends_with(sid)),
            "in-flight session must NOT be re-dispatched (this was the duplicate-prompt bug)"
        );

        // an expired lease (crashed/zombie pass) must self-heal, not wedge forever
        claim_inflight(
            &sessions,
            "claude",
            &[sid.to_string()],
            epoch_now().saturating_sub(1),
        );
        let after = eligible_sessions(&sessions, Duration::ZERO, 10, "claude").unwrap();
        assert!(
            after.iter().any(|p| p.ends_with(sid)),
            "expired lease must stop excluding"
        );

        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn iso_to_epoch_parses_ledger_timestamps() {
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch("2000-01-01T00:00:00Z"), Some(946_684_800));
        // fractional seconds tolerated (ignored)
        assert_eq!(iso_to_epoch("2000-01-01T00:00:00.123Z"), Some(946_684_800));
        assert_eq!(iso_to_epoch("not a date"), None);
        assert_eq!(iso_to_epoch(""), None);
    }

    /// A session resumed after sealing: raw turns.jsonl beside turns.jsonl.zst. A ledger
    /// entry only covers the resumed content if it postdates the sealed generation
    /// (.zst mtime); older entries mean the learner never saw the new turns.
    #[test]
    fn resumed_session_is_relearned_per_generation() {
        let root = std::env::temp_dir().join(format!("plant-resume-test-{}", std::process::id()));
        let sessions = root.join("sessions");
        let sid = "resumed-after-seal";
        let dir = sessions.join("2026/07/14").join(sid);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
        std::fs::write(dir.join("turns.jsonl.zst"), "gen1-sealed").unwrap();
        // seal boundary: 2026-07-14T17:42:41Z
        let sealed_at = 1_784_050_961u64;
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("turns.jsonl.zst"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(sealed_at))
            .unwrap();
        std::fs::create_dir_all(root.join("learnings")).unwrap();
        // claude ledgered BEFORE the seal boundary, codex AFTER it
        std::fs::write(
            root.join("learnings/.ledger.jsonl"),
            format!(
                concat!(
                    "{{\"session_id\":\"{sid}\",\"processed_at\":\"2026-07-14T15:08:26Z\"}}\n",
                    "{{\"session_id\":\"{sid}\",\"learner\":\"codex\",",
                    "\"processed_at\":\"2026-07-16T18:53:27Z\"}}\n"
                ),
                sid = sid
            ),
        )
        .unwrap();

        // watchdog: codex covered the resumed generation, claude did not
        let stuck = stuck_captures(&sessions, Duration::ZERO).unwrap();
        let entry = stuck.iter().find(|s| s.sid == sid).expect("reported stuck");
        assert_eq!(entry.state, "half-learned:claude");

        // eligibility mirrors that: claude re-learns, codex does not
        let claude = eligible_sessions(&sessions, Duration::ZERO, 10, "claude").unwrap();
        let codex = eligible_sessions(&sessions, Duration::ZERO, 10, "codex").unwrap();
        assert!(claude.iter().any(|p| p.ends_with(sid)));
        assert!(!codex.iter().any(|p| p.ends_with(sid)));

        // a sealed-only capture keeps historic semantics: entry counts as-is
        std::fs::remove_file(dir.join("turns.jsonl")).unwrap();
        let inventory = vaultr::vault::CaptureGenerations::load(&dir).unwrap();
        let sealed = SessionGeneration::current(sid.to_string(), inventory).unwrap();
        assert_eq!(sealed.selected, GenerationKind::Sealed);
        assert!(sealed.learned_current(&ledger_latest(&sessions, "claude")));

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn anchored_detach_and_cleanup_reject_preoperation_entry_swaps() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("plant-anchored-swap-{}", uuid::Uuid::new_v4()));
        let session = root.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"outside evidence\n").unwrap();
        let outside_before = std::fs::read(&outside).unwrap();
        let directory = AnchoredDirectory::open(&session).unwrap();

        let raw_name = "turns.jsonl";
        std::fs::write(session.join(raw_name), b"capture evidence\n").unwrap();
        let raw = directory.open_required(raw_name, true).unwrap();
        std::fs::rename(session.join(raw_name), session.join("retained-turns")).unwrap();
        symlink(&outside, session.join(raw_name)).unwrap();

        assert!(directory
            .replace_entry(raw_name, "turns.jsonl.sealing-0-deadbeef", &raw, None)
            .is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
        assert!(!session.join("turns.jsonl.sealing-0-deadbeef").exists());

        let detached_name = "herdr.jsonl.sealing-0-deadbeef";
        std::fs::write(session.join(detached_name), b"herdr evidence\n").unwrap();
        let detached = directory.open_required(detached_name, false).unwrap();
        std::fs::rename(session.join(detached_name), session.join("retained-herdr")).unwrap();
        symlink(&outside, session.join(detached_name)).unwrap();

        assert!(directory.unlink_if_same(detached_name, &detached).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
        assert!(std::fs::symlink_metadata(session.join(detached_name))
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn session_flock_serializes_independent_directory_owners() {
        let root =
            std::env::temp_dir().join(format!("plant-session-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let first = AnchoredDirectory::open(&root).unwrap();
        let second = AnchoredDirectory::open(&root).unwrap();
        first.lock_exclusive().unwrap();

        // A separate open file description is the same boundary another
        // cooperating process obtains. It cannot enter while `first` is held.
        // SAFETY: `second` retains its valid directory fd for this call.
        assert_ne!(
            unsafe { libc::flock(second.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
            0
        );
        assert!(matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        ));

        drop(first);
        // SAFETY: `second` still retains its valid directory fd.
        assert_eq!(
            unsafe { libc::flock(second.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
            0
        );
        drop(second);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn detachment_rejects_symlinked_capture_and_sidecar_sources() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("plant-detach-symlink-{}", uuid::Uuid::new_v4()));
        let session = root.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"outside evidence\n").unwrap();
        let outside_before = std::fs::read(&outside).unwrap();

        symlink(&outside, session.join("turns.jsonl")).unwrap();
        assert!(detach_capture_generation(&session).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
        assert!(!std::fs::read_dir(&session)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(String::from))
            .any(|name| name.starts_with("turns.jsonl.sealing-")));

        std::fs::remove_file(session.join("turns.jsonl")).unwrap();
        let herdr = session.join("herdr.jsonl");
        let herdr_dest = session.join("herdr.jsonl.zst");
        symlink(&outside, &herdr).unwrap();
        assert!(detach_sidecar(&herdr, &herdr_dest).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
        assert!(!std::fs::read_dir(&session)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(String::from))
            .any(|name| name.starts_with("herdr.jsonl.sealing-")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sealing_rejects_symlinked_source_and_destination_without_mutating_targets() {
        use std::os::unix::fs::symlink;

        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("plant-seal-symlink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("turns.jsonl.sealing-0-source");
        let destination = root.join("turns.jsonl.zst");
        let outside_source = root.join("outside-source");
        let outside_destination = root.join("outside-destination");
        std::fs::write(&outside_source, b"outside source\n").unwrap();
        std::fs::write(&outside_destination, b"outside destination\n").unwrap();
        let source_before = std::fs::read(&outside_source).unwrap();
        let destination_before = std::fs::read(&outside_destination).unwrap();
        symlink(&outside_source, &source).unwrap();
        let generation = vaultr::vault::DetachedGeneration {
            path: source.clone(),
            base_len: 0,
            digest: vaultr::vault::sha256_file(&outside_source).unwrap(),
        };

        assert!(seal_generation(&generation, &destination).await.is_err());
        assert_eq!(std::fs::read(&outside_source).unwrap(), source_before);
        assert!(!destination.exists());

        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"detached evidence\n").unwrap();
        symlink(&outside_destination, &destination).unwrap();
        let generation = vaultr::vault::DetachedGeneration {
            path: source.clone(),
            base_len: 0,
            digest: vaultr::vault::sha256_file(&source).unwrap(),
        };

        assert!(seal_generation(&generation, &destination).await.is_err());
        assert_eq!(
            std::fs::read(&outside_destination).unwrap(),
            destination_before
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"detached evidence\n");
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str().map(String::from))
                .all(|name| !name.contains(".frame-") && !name.contains(".merged-")),
            "failed sealing removes only its descriptor-owned temps"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sealing_restart_cleans_only_exact_temp_debris_and_rejects_near_misses() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("plant-seal-temp-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let capture_bytes = b"capture generation\n";
        let capture_digest = vaultr::vault::sha256_hex(capture_bytes);
        let capture_source = root.join(format!("turns.jsonl.sealing-0-{capture_digest}"));
        let capture_destination = root.join("turns.jsonl.zst");
        std::fs::write(&capture_source, capture_bytes).unwrap();
        let exact_frame = root.join(format!(".turns.jsonl.zst.frame-{}", uuid::Uuid::new_v4()));
        let exact_merged = root.join(format!(".turns.jsonl.zst.merged-{}", uuid::Uuid::new_v4()));
        std::fs::write(&exact_frame, b"partial compressed debris").unwrap();
        std::fs::write(&exact_merged, b"partial merged debris").unwrap();
        let capture = vaultr::vault::DetachedGeneration {
            path: capture_source,
            base_len: 0,
            digest: capture_digest,
        };

        seal_generation(&capture, &capture_destination)
            .await
            .unwrap();

        assert!(!exact_frame.exists());
        assert!(!exact_merged.exists());
        assert_eq!(
            zstd::decode_all(std::fs::File::open(&capture_destination).unwrap()).unwrap(),
            capture_bytes
        );

        let herdr_bytes = b"herdr generation\n";
        let herdr_digest = vaultr::vault::sha256_hex(herdr_bytes);
        let herdr_source = root.join(format!("herdr.jsonl.sealing-0-{herdr_digest}"));
        let herdr_destination = root.join("herdr.jsonl.zst");
        std::fs::write(&herdr_source, herdr_bytes).unwrap();
        let near_miss = root.join(".herdr.jsonl.zst.frame-not-a-canonical-v4-uuid");
        let near_miss_bytes = b"unrecognized evidence";
        std::fs::write(&near_miss, near_miss_bytes).unwrap();
        let herdr = vaultr::vault::DetachedGeneration {
            path: herdr_source.clone(),
            base_len: 0,
            digest: herdr_digest,
        };

        assert!(seal_generation(&herdr, &herdr_destination).await.is_err());

        assert_eq!(std::fs::read(&near_miss).unwrap(), near_miss_bytes);
        assert_eq!(std::fs::read(&herdr_source).unwrap(), herdr_bytes);
        assert!(!herdr_destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_temp_upgrade_removes_only_enumerated_regular_debris() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("plant-legacy-temp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let turns = root.join("turns.jsonl");
        std::fs::write(&turns, b"capture generation\n").unwrap();
        std::fs::write(root.join("turns.scrub-tmp"), b"partial scrub debris").unwrap();
        assert!(scrub(&turns).await);
        assert!(!root.join("turns.scrub-tmp").exists());

        let capture = detach_capture_generation(&root).unwrap();
        for name in ["turns.jsonl.frame-tmp", "turns.jsonl.zst-tmp"] {
            std::fs::write(root.join(name), b"partial capture seal debris").unwrap();
        }
        seal_generation(&capture, &root.join("turns.jsonl.zst"))
            .await
            .unwrap();
        assert!(!root.join("turns.jsonl.frame-tmp").exists());
        assert!(!root.join("turns.jsonl.zst-tmp").exists());

        let herdr = root.join("herdr.jsonl");
        let herdr_dest = root.join("herdr.jsonl.zst");
        std::fs::write(&herdr, b"sidecar generation\n").unwrap();
        let herdr_generation = detach_sidecar(&herdr, &herdr_dest).unwrap().unwrap();
        for name in ["herdr.jsonl.frame-tmp", "herdr.jsonl.zst-tmp"] {
            std::fs::write(root.join(name), b"partial sidecar seal debris").unwrap();
        }
        seal_generation(&herdr_generation, &herdr_dest)
            .await
            .unwrap();
        assert!(!root.join("herdr.jsonl.frame-tmp").exists());
        assert!(!root.join("herdr.jsonl.zst-tmp").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_temp_upgrade_rejects_symlink_nonregular_and_near_miss_evidence() {
        use std::os::unix::fs::symlink;

        let legacy = &["turns.jsonl.frame-tmp", "turns.jsonl.zst-tmp"];
        for case in ["symlink", "directory", "near-miss"] {
            let root = std::env::temp_dir()
                .join(format!("plant-legacy-temp-{case}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let retained = root.join("turns.jsonl.zst-tmp");
            std::fs::write(&retained, b"exact regular evidence").unwrap();
            let outside = root.with_extension(format!("{case}-outside"));
            std::fs::write(&outside, b"outside evidence").unwrap();
            let outside_before = std::fs::read(&outside).unwrap();
            let suspect = match case {
                "symlink" => {
                    let path = root.join("turns.jsonl.frame-tmp");
                    symlink(&outside, &path).unwrap();
                    path
                }
                "directory" => {
                    let path = root.join("turns.jsonl.frame-tmp");
                    std::fs::create_dir(&path).unwrap();
                    path
                }
                "near-miss" => {
                    let path = root.join("turns.jsonl.frame-tm");
                    std::fs::write(&path, b"near-miss evidence").unwrap();
                    path
                }
                _ => unreachable!(),
            };
            let directory = AnchoredDirectory::open(&root).unwrap();
            directory.lock_exclusive().unwrap();
            assert!(directory
                .cleanup_temps("turns.jsonl.zst", &["frame", "merged"], legacy)
                .is_err());
            assert!(std::fs::symlink_metadata(&suspect).is_ok());
            assert_eq!(std::fs::read(&retained).unwrap(), b"exact regular evidence");
            assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
            drop(directory);
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_file(outside);
        }
    }

    #[tokio::test]
    async fn corrupt_successful_compression_never_retires_detached_evidence() {
        let root =
            std::env::temp_dir().join(format!("plant-corrupt-compressor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let body = b"sole detached evidence\n";
        let digest = vaultr::vault::sha256_hex(body);
        let source = root.join(format!("turns.jsonl.sealing-0-{digest}"));
        let destination = root.join("turns.jsonl.zst");
        std::fs::write(&source, body).unwrap();
        let generation = vaultr::vault::DetachedGeneration {
            path: source.clone(),
            base_len: 0,
            digest,
        };

        let error =
            seal_generation_with(&generation, &destination, FrameCompressor::CorruptSuccess)
                .await
                .unwrap_err();

        assert!(error.contains(&destination.display().to_string()));
        assert!(!error.contains("sole detached evidence"));
        assert_eq!(std::fs::read(&source).unwrap(), body);
        assert!(destination.exists(), "bad commit remains diagnosable");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sealing_retry_accepts_a_different_valid_frame_representation() {
        let root = std::env::temp_dir().join(format!("plant-valid-frame-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let base = zstd::encode_all("prior generation\n".as_bytes(), 7).unwrap();
        let body = b"detached generation\n";
        let digest = vaultr::vault::sha256_hex(body);
        let source = root.join(format!("turns.jsonl.sealing-{}-{digest}", base.len()));
        let destination = root.join("turns.jsonl.zst");
        std::fs::write(&source, body).unwrap();
        let mut committed = base.clone();
        committed.extend(zstd::encode_all(body.as_slice(), 1).unwrap());
        std::fs::write(&destination, &committed).unwrap();
        std::fs::File::open(&destination)
            .unwrap()
            .sync_all()
            .unwrap();
        AnchoredDirectory::open(&root).unwrap().sync().unwrap();
        let generation = vaultr::vault::DetachedGeneration {
            path: source.clone(),
            base_len: base.len() as u64,
            digest,
        };

        seal_generation(&generation, &destination).await.unwrap();

        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), committed);
        assert_eq!(
            zstd::decode_all(std::fs::File::open(&destination).unwrap()).unwrap(),
            b"prior generation\ndetached generation\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compressor_timeout_kills_and_reaps_the_child() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as libc::pid_t;

        let error = wait_for_compressor(&mut child, Duration::from_millis(20))
            .await
            .unwrap_err();

        assert!(error.contains("killed and reaped"), "{error}");
        assert!(child.id().is_none(), "wait retained an unreaped child");
        // SAFETY: signal 0 performs no mutation and only probes the former pid.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    /// Sealing a resumed sidecar appends a second zstd frame; zstd -d reads the
    /// concatenation back as gen1 + gen2. Requires zstd on PATH (skips otherwise).
    #[tokio::test]
    async fn detached_sidecar_appends_frames_for_resumed_sessions() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("plant-seal-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let raw = root.join("herdr.jsonl");
        let dest = root.join("herdr.jsonl.zst");

        std::fs::write(&raw, "gen1-line\n").unwrap();
        let first = detach_sidecar(&raw, &dest).unwrap().unwrap();
        seal_generation(&first, &dest).await.expect("first seal");
        assert!(!raw.exists(), "raw removed after seal");
        assert!(dest.is_file());

        // resume: raw reappears with new content only
        std::fs::write(&raw, "gen2-line\n").unwrap();
        let second = detach_sidecar(&raw, &dest).unwrap().unwrap();
        seal_generation(&second, &dest).await.expect("merge seal");
        assert!(!raw.exists());

        let out = std::process::Command::new("zstd")
            .args(["-d", "-c", dest.to_str().unwrap()])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "gen1-line\ngen2-line\n",
            "both generations survive in order"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn detached_generation_retry_after_durable_destination_rename_is_exactly_once() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("plant-detached-seal-{}", uuid::Uuid::new_v4()));
        let vault = root.join("sessions");
        let sid = "detached-generation";
        let dir = vault.join("2026/07/20").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("turns.jsonl.zst");

        std::fs::write(dir.join("turns.jsonl"), "generation-one\n").unwrap();
        let first = crate::capture::detach_generation(&vault, sid, &dir)
            .await
            .unwrap()
            .unwrap();
        seal_generation(&first, &dest).await.unwrap();

        std::fs::write(dir.join("turns.jsonl"), "generation-two\n").unwrap();
        let second = crate::capture::detach_generation(&vault, sid, &dir)
            .await
            .unwrap()
            .unwrap();
        let frame = dir.join("manual-frame.zst");
        let result = run(
            &[
                "zstd",
                "-19",
                "-T0",
                "-q",
                "-f",
                "-o",
                frame.to_str().unwrap(),
                second.path.to_str().unwrap(),
            ],
            Duration::from_secs(60),
        )
        .await;
        assert!(
            result.ok,
            "fixture compression: {}",
            result.failure_detail()
        );
        let merged = dir.join("manual-merged.zst");
        {
            use std::io::Write;
            let mut out = std::fs::File::create(&merged).unwrap();
            out.write_all(&std::fs::read(&dest).unwrap()).unwrap();
            out.write_all(&std::fs::read(&frame).unwrap()).unwrap();
            out.sync_all().unwrap();
        }
        std::fs::rename(&merged, &dest).unwrap();
        // Fault boundary: the committed destination name is directory-durable,
        // but retained detached evidence has not been removed.
        AnchoredDirectory::open(&dir).unwrap().sync().unwrap();

        seal_generation(&second, &dest).await.unwrap();
        assert!(!second.path.exists(), "committed transaction cleaned");
        let decoded = zstd::decode_all(std::fs::File::open(&dest).unwrap()).unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "generation-one\ngeneration-two\n"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn herdr_retry_after_durable_destination_rename_is_byte_exactly_once() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("plant-herdr-seal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let raw = root.join("herdr.jsonl");
        let dest = root.join("herdr.jsonl.zst");

        std::fs::write(&raw, "generation-one\n").unwrap();
        let first = detach_sidecar(&raw, &dest).unwrap().unwrap();
        seal_generation(&first, &dest).await.unwrap();

        std::fs::write(&raw, "generation-two\n").unwrap();
        let second = detach_sidecar(&raw, &dest).unwrap().unwrap();
        let frame = root.join("manual-herdr-frame.zst");
        let result = run(
            &[
                "zstd",
                "-19",
                "-T0",
                "-q",
                "-f",
                "-o",
                frame.to_str().unwrap(),
                second.path.to_str().unwrap(),
            ],
            Duration::from_secs(60),
        )
        .await;
        assert!(
            result.ok,
            "fixture compression: {}",
            result.failure_detail()
        );
        let merged = root.join("manual-herdr-merged.zst");
        {
            use std::io::Write;
            let mut out = std::fs::File::create(&merged).unwrap();
            out.write_all(&std::fs::read(&dest).unwrap()).unwrap();
            out.write_all(&std::fs::read(&frame).unwrap()).unwrap();
            out.sync_all().unwrap();
        }
        std::fs::rename(&merged, &dest).unwrap();
        // Fault boundary: the committed destination name is directory-durable,
        // but retained detached evidence has not been removed.
        AnchoredDirectory::open(&root).unwrap().sync().unwrap();
        let committed = std::fs::read(&dest).unwrap();

        seal_generation(&second, &dest).await.unwrap();

        assert!(
            !second.path.exists(),
            "retry only cleaned detached evidence"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), committed);
        let decoded = zstd::decode_all(std::fs::File::open(&dest).unwrap()).unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "generation-one\ngeneration-two\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn detached_sealing_conflict_is_an_operational_failure() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("plant-detached-conflict-{}", uuid::Uuid::new_v4()));
        let vault = root.join("sessions");
        let dir = vault.join("2026/07/20/conflict");
        std::fs::create_dir_all(&dir).unwrap();
        let body = b"detached evidence\n";
        let detached = dir.join(format!(
            "turns.jsonl.sealing-0-{}",
            vaultr::vault::sha256_hex(body)
        ));
        std::fs::write(&detached, body).unwrap();
        let sealed = dir.join("turns.jsonl.zst");
        let conflict = zstd::encode_all("different generation\n".as_bytes(), 3).unwrap();
        std::fs::write(&sealed, &conflict).unwrap();

        let error = compress_sweep(&vault, Duration::ZERO).await.unwrap_err();
        assert!(error.contains("seal detached generation"), "{error}");
        assert!(detached.exists(), "detached evidence is preserved");
        assert_eq!(std::fs::read(&sealed).unwrap(), conflict);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn job_sid_registry_parses_lines() {
        let f = std::env::temp_dir().join(format!("plant-jobsids-{}", std::process::id()));
        std::fs::write(&f, "aaa-111\n\n  bbb-222  \n").unwrap();
        let sids = job_sids_at(&f);
        assert!(sids.contains("aaa-111") && sids.contains("bbb-222"));
        assert_eq!(sids.len(), 2);
        assert!(job_sids_at(Path::new("/nonexistent/registry")).is_empty());
        let _ = std::fs::remove_file(f);
    }
    #[test]
    fn oversized_seal_is_gitignored_idempotently() {
        let root = std::env::temp_dir().join(format!("plant-cap-test-{}", std::process::id()));
        let sessions = root.join("sessions");
        let dir = sessions.join("2026/07/18/big-one");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        let sealed = dir.join("turns.jsonl.zst");
        std::fs::write(&sealed, "blob").unwrap();

        exclude_from_commit(&sessions, &sealed, 2_700_000_000);
        exclude_from_commit(&sessions, &sealed, 2_700_000_000); // idempotent
        let gi = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        let line = "sessions/2026/07/18/big-one/turns.jsonl.zst";
        assert_eq!(
            gi.matches(line).count(),
            1,
            "exactly one ignore line:\n{gi}"
        );
        assert!(gi.contains("2.7GB"));
        assert!(sealed.is_file(), "capture data never touched");

        let _ = std::fs::remove_dir_all(root);
    }
}
