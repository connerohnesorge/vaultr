//! Session-sweep primitives: eligibility discovery, scrub, compress. Orchestration
//! (scheduling, agent panes) lives in jobs.rs; these are exposed as `plant sessions
//! eligible` / `plant compress once` subcommands and called directly by the built-in Rust jobs.
//! Every failure path non-fatal: capture uptime is sacred. All heavy work shells out.

use crate::domain::Harness;
use crate::process::{run, run30, which};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// sid -> latest `processed_at` (epoch secs; 0 when the entry has none) for `learner`.
/// Entries without a `learner` key are the legacy Claude pass.
fn ledger_latest(vault: &Path, learner: Harness) -> HashMap<String, u64> {
    let mut processed = HashMap::new();
    let Ok(root) = vaultr::validate::content_root(vault) else {
        return processed; // rootless sessions path => nothing ledgered; non-fatal
    };
    if let Ok(text) = std::fs::read_to_string(vaultr::validate::ledger_path(&root)) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let recorded = v.get("learner").and_then(|s| s.as_str());
                if recorded == Some(learner.ledger_label())
                    || (recorded.is_none() && learner == Harness::ClaudeCode)
                {
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

/// The sealed sibling (`turns.jsonl` -> `turns.jsonl.zst`) of a raw capture file.
fn seal_sibling(raw: &Path) -> PathBuf {
    let name = raw.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    raw.with_file_name(format!("{name}.zst"))
}

/// Has `learner` learned the *current* content behind `path`? Plain contains-check,
/// except for a raw file with a sealed sibling — a session resumed after sealing —
/// where the ledger entry only counts if it postdates the sealed content (zstd
/// preserves the source mtime, so the .zst mtime is the last write of what was
/// learned+sealed). Sealed-only and never-sealed paths keep the historic semantics:
/// pre-existing ledger entries stay valid and history is never re-opened.
fn learned_current(latest: &HashMap<String, u64>, sid: &str, path: &Path) -> bool {
    let Some(&ts) = latest.get(sid) else {
        return false;
    };
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        return true;
    }
    match std::fs::metadata(seal_sibling(path))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    {
        None => true, // no prior seal: any entry covers the file
        Some(sealed) => ts > sealed.as_secs(),
    }
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn inflight_path(vault: &Path, learner: Harness) -> Result<PathBuf, String> {
    let root = vaultr::validate::content_root(vault).map_err(|e| e.to_string())?;
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let lease: InflightLease =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if epoch_now() >= lease.expires_at {
        return Ok(None);
    }
    if lease.sids.is_empty() {
        return Err(format!("{} has an empty active batch", path.display()));
    }
    Ok(Some(lease))
}

/// Sessions a still-running learn pass has claimed for `learner`, if the lease hasn't
/// expired. The ledger is only written when a pass *completes*, so without this the next
/// scheduler tick re-selects the same not-yet-ledgered batch and double-learns it (the
/// duplicate-prompt bug). ponytail: one file per learner, expiry self-heals a crashed or
/// timed-out pass — no explicit release path to leak.
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
    let body = serde_json::to_vec(lease).map_err(|e| e.to_string())?;
    crate::fsutil::atomic_replace(path, &body)
        .map_err(|e| format!("publish {}: {e}", path.display()))
}

fn capture_files(vault: &Path) -> Vec<(String, PathBuf)> {
    turns_files(vault, true)
}

/// YYYY/MM/DD/<id>/turns.jsonl[.zst] under vault -> (session_id, path).
/// Uses the shared digit-filtered walker, so non-date dirs (e.g. `.meta`) are skipped.
fn turns_files(vault: &Path, include_compressed: bool) -> Vec<(String, PathBuf)> {
    let mut out = vec![];
    for (sid, sess) in vaultr::vault::walk_session_dirs(vault) {
        let f = if include_compressed {
            vaultr::vault::capture_file(&sess).ok()
        } else {
            let raw = sess.join("turns.jsonl");
            raw.is_file().then_some(raw)
        };
        if let Some(f) = f {
            out.push((sid, f));
        }
    }
    out
}

fn idle_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
}

fn idle_for(path: &Path, idle: Duration) -> bool {
    idle_secs(path)
        .map(|s| s >= idle.as_secs())
        .unwrap_or(false)
}

/// Learn substance gate: >20KB, or >5 turns (only read small files to count).
/// Compressed captures already cleared the legacy Claude pass before sealing.
fn substantive(path: &Path) -> bool {
    let compressed = path.extension().and_then(|e| e.to_str()) == Some("zst");
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    compressed
        || size > 20_480
        || std::fs::read_to_string(path)
            .map(|t| t.trim_end().lines().count() > 5)
            .unwrap_or(false)
}

/// One raw Session Capture the cultivation pipeline should have sealed by now.
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

/// Classify raw captures idle >= `age` by learn-ledger state. Read-only; the watchdog
/// job and `plant sessions stuck` both call this. sub-threshold captures can never seal
/// under current rules (learn skips them, sealing needs both ledgers) — reported so the
/// gap stays visible, but callers treat them as informational, not actionable.
pub fn stuck_captures(vault: &Path, age: Duration) -> Vec<StuckCapture> {
    let claude = ledger_latest(vault, Harness::ClaudeCode);
    let codex = ledger_latest(vault, Harness::Codex);
    let mut out = vec![];
    let jobs = job_sids();
    for (sid, path) in turns_files(vault, false) {
        let Some(idle) = idle_secs(&path) else {
            continue;
        };
        if idle < age.as_secs() {
            continue;
        }
        let state = if jobs.contains(&sid) {
            StuckState::JobCapture
        } else {
            match (
                learned_current(&claude, &sid, &path),
                learned_current(&codex, &sid, &path),
            ) {
                (true, true) => StuckState::SealBlocked,
                (true, false) => StuckState::HalfLearned(Harness::Codex),
                (false, true) => StuckState::HalfLearned(Harness::ClaudeCode),
                (false, false) if substantive(&path) => StuckState::Unlearned,
                (false, false) => StuckState::SubThreshold,
            }
        };
        out.push(StuckCapture {
            sid,
            state,
            idle_secs: idle,
        });
    }
    out
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

fn eligible_candidates(
    vault: &Path,
    idle: Duration,
    max: usize,
    learner: Harness,
) -> Vec<(String, PathBuf)> {
    let processed = ledger_latest(vault, learner);
    let inflight = inflight_sessions(vault, learner);
    let jobs = job_sids();
    let mut out = vec![];
    for (sid, path) in capture_files(vault) {
        if jobs.contains(&sid)
            || learned_current(&processed, &sid, &path)
            || inflight.contains(&sid)
            || !idle_for(&path, idle)
        {
            continue;
        }
        if substantive(&path) {
            if let Some(dir) = path.parent() {
                out.push((sid, dir.to_path_buf()));
            }
        }
    }
    out.truncate(max);
    out
}

pub fn eligible_sessions(
    vault: &Path,
    idle: Duration,
    max: usize,
    learner: Harness,
) -> Vec<PathBuf> {
    eligible_candidates(vault, idle, max, learner)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// Select and atomically lease one batch while holding a learner-scoped cross-process
/// lock. An active unexpired batch wins; malformed or unpublishable state fails closed.
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
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    if read_inflight(&path)?.is_some() {
        return Ok(Vec::new());
    }
    let batch = eligible_candidates(vault, idle, max, learner);
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

/// Diagnostics for the `sessions eligible` subcommand's stderr — stdout must stay
/// clean (it is substituted into an agent prompt by the learn job).
pub fn eligibility_stats(vault: &Path, learner: Harness) -> (usize, usize) {
    (
        capture_files(vault).len(),
        ledger_latest(vault, learner).len(),
    )
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

/// Redact known-secret patterns + a literal denylist in place. false => do not compress/push.
/// Rust-native (regex): no subprocess, no timeout, constant memory. Streaming line-by-line —
/// turns.jsonl files reach GBs, and whole-file reads were the historic multi-GB RSS spike
/// (and the Bun-era jetsam death loop).
pub async fn scrub(path: &Path) -> bool {
    let path_s = path.display().to_string();

    // optional literal denylist: known plaintext secrets no regex can pattern-match
    // (e.g. the CLAUDE.md athens password). Absent file => regex-only.
    let mut needles: HashSet<String> = HashSet::new();
    let denylist = format!(
        "{}/.config/wireproxy/scrub-denylist.txt",
        std::env::var("HOME").unwrap_or_default()
    );
    if let Ok(contents) = std::fs::read_to_string(&denylist) {
        for line in contents.lines() {
            let s = line.trim();
            if s.len() >= 6 {
                // match both raw and JSON-escaped forms
                let j = serde_json::to_string(s).unwrap_or_default();
                needles.insert(s.to_string());
                if j.len() >= 2 {
                    needles.insert(j[1..j.len() - 1].to_string());
                }
            }
        }
    }

    let patterns = secret_regexes();

    // rewrite line-by-line via temp file + rename: memory = one line, not one file
    let tmp = path.with_extension("scrub-tmp");
    let hits = (|| -> std::io::Result<usize> {
        use std::io::{BufRead, Write};
        let reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut writer = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut hits = 0;
        for line in reader.lines() {
            let (line, n) = redact_line(line?, &needles, &patterns);
            hits += n;
            writeln!(writer, "{line}")?;
        }
        writer.flush()?;
        Ok(hits)
    })();
    let hits = match hits {
        Ok(h) => h,
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
    };
    if hits > 0 {
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        let rel = path_s.split("/sessions/").nth(1).unwrap_or(&path_s);
        println!("[scrub] {rel}: {hits} redaction(s)");
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    true
}

/// Seal `raw` into its `.zst` sibling and remove `raw`. When the sibling already exists
/// (a session resumed after sealing), the new frame is APPENDED — concatenated zstd
/// frames are a valid stream that zstdcat reads transparently, so both generations
/// survive losslessly in chronological order. Atomic: the merged file is assembled in a
/// temp sibling and renamed over the destination; the raw file is only removed after.
/// The destination mtime is set to the raw's mtime (as `zstd` itself would), because
/// learned_current uses it as the generation boundary.
async fn seal_file(raw: &Path) -> Result<(), String> {
    let dest = seal_sibling(raw);
    let raw_mtime = std::fs::metadata(raw)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat: {e}"))?;
    let frame = raw.with_extension("frame-tmp");
    let raw_s = raw.display().to_string();
    let frame_s = frame.display().to_string();
    let zstd = run(
        &["zstd", "-19", "-T0", "-q", "-f", "-o", &frame_s, &raw_s],
        Duration::from_secs(600),
    )
    .await;
    if !zstd.ok {
        let _ = std::fs::remove_file(&frame);
        return Err(zstd.failure_detail());
    }
    let merged = raw.with_extension("zst-tmp");
    let assemble = (|| -> std::io::Result<()> {
        let mut out = std::fs::File::create(&merged)?;
        if dest.is_file() {
            std::io::copy(&mut std::fs::File::open(&dest)?, &mut out)?;
        }
        std::io::copy(&mut std::fs::File::open(&frame)?, &mut out)?;
        out.sync_all()?;
        std::fs::rename(&merged, &dest)?;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&dest)?
            .set_modified(raw_mtime)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&frame);
    match assemble {
        Ok(()) => {
            std::fs::remove_file(raw).map_err(|e| format!("rm raw: {e}"))?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&merged);
            Err(e.to_string())
        }
    }
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

pub async fn compress_sweep(vault: &Path, idle: Duration) -> bool {
    if !which("zstd") {
        eprintln!("[compress] zstd not on PATH");
        return false;
    }
    let claude = ledger_latest(vault, Harness::ClaudeCode);
    let codex = ledger_latest(vault, Harness::Codex);
    let jobs = job_sids();
    let mut sealed = 0u32;
    for (sid, path) in turns_files(vault, false) {
        // job self-captures seal without waiting for learn — they are never dispatched
        let learned = learned_current(&claude, &sid, &path) && learned_current(&codex, &sid, &path);
        if !(learned || jobs.contains(&sid)) || !idle_for(&path, idle) {
            continue;
        }
        // Never seal a raw generation with an open reservation or undrained stage:
        // the delta lineage isn't final until every prepared sequence has drained.
        if crate::capture::has_open_capture(vault, &sid) {
            continue;
        }
        if !scrub(&path).await {
            continue; // unscrubbed data must not leave the machine
        }
        let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let path_s = path.display().to_string();
        let herdr = path.with_file_name("herdr.jsonl");
        if herdr.is_file() {
            if let Err(e) = seal_file(&herdr).await {
                eprintln!("[compress] {sid}: herdr.jsonl seal skipped ({e}); next sweep retries");
                continue;
            }
        }
        match seal_file(&path).await {
            Ok(()) => {
                sealed += 1;
                let after = std::fs::metadata(format!("{path_s}.zst"))
                    .map(|m| m.len())
                    .unwrap_or(0);
                if after > COMMIT_CAP {
                    exclude_from_commit(vault, &seal_sibling(&path), after);
                }
                let rel = path_s.split("/sessions/").nth(1).unwrap_or(&path_s);
                println!(
                    "[compress] {rel}: {:.1}MB -> {:.1}MB",
                    before as f64 / 1e6,
                    after as f64 / 1e6
                );
            }
            Err(e) => {
                eprintln!("[compress] {sid}: turns.jsonl seal skipped ({e}); next sweep retries");
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
                return true;
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let claude = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::ClaudeCode);
        let codex = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::Codex);
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

        let raw = turns_files(&sessions, false);
        assert_eq!(raw.len(), 1, "only the dated session, got {raw:?}");
        assert_eq!(raw[0].0, sid);
        assert_eq!(raw[0].1, dated.join("turns.jsonl"));

        // include_compressed also picks up sealed sessions, still date-filtered.
        std::fs::remove_file(dated.join("turns.jsonl")).unwrap();
        std::fs::write(dated.join("turns.jsonl.zst"), "sealed").unwrap();
        let all = turns_files(&sessions, true);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, dated.join("turns.jsonl.zst"));

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

        let stuck = stuck_captures(&sessions, Duration::ZERO);
        let state = |sid: &str| {
            stuck
                .iter()
                .find(|s| s.sid == sid)
                .map(|s| s.state)
                .unwrap_or_else(|| {
                    panic!(
                        "{sid} missing from {:?}",
                        stuck.iter().map(|s| &s.sid).collect::<Vec<_>>()
                    )
                })
        };
        assert_eq!(state("both-ledgered"), StuckState::SealBlocked);
        assert_eq!(
            state("claude-only"),
            StuckState::HalfLearned(Harness::Codex)
        );
        assert_eq!(
            state("codex-only"),
            StuckState::HalfLearned(Harness::ClaudeCode)
        );
        assert_eq!(state("nobody-big"), StuckState::Unlearned);
        assert_eq!(state("nobody-small"), StuckState::SubThreshold);
        assert!(!stuck.iter().any(|s| s.sid == "already-sealed"));

        // freshly written files are idle ~0s: an age gate must exempt them all
        assert!(stuck_captures(&sessions, Duration::from_secs(3600)).is_empty());

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

        // tick 1: select and durably claim one batch before returning it
        let first = eligible_and_claim(
            &sessions,
            Duration::ZERO,
            10,
            Harness::ClaudeCode,
            Duration::from_secs(3600),
        )
        .unwrap();
        assert!(
            first.iter().any(|p| p.ends_with(sid)),
            "un-ledgered session should be eligible"
        );

        // tick 2 while the pass is still running: one active learner batch wins
        let during = eligible_and_claim(
            &sessions,
            Duration::ZERO,
            10,
            Harness::ClaudeCode,
            Duration::from_secs(3600),
        )
        .unwrap();
        assert!(
            during.is_empty(),
            "in-flight session must NOT be re-dispatched (this was the duplicate-prompt bug)"
        );

        // an expired lease (crashed/zombie pass) must self-heal, not wedge forever
        publish_inflight(
            &inflight_path(&sessions, Harness::ClaudeCode).unwrap(),
            &InflightLease {
                sids: vec![sid.to_string()],
                expires_at: epoch_now().saturating_sub(1),
            },
        )
        .unwrap();
        let after = eligible_and_claim(
            &sessions,
            Duration::ZERO,
            10,
            Harness::ClaudeCode,
            Duration::from_secs(3600),
        );
        assert!(
            after.unwrap().iter().any(|p| p.ends_with(sid)),
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
        let stuck = stuck_captures(&sessions, Duration::ZERO);
        let entry = stuck.iter().find(|s| s.sid == sid).expect("reported stuck");
        assert_eq!(entry.state, StuckState::HalfLearned(Harness::ClaudeCode));

        // eligibility mirrors that: claude re-learns, codex does not
        let claude = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::ClaudeCode);
        let codex = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::Codex);
        assert!(claude.iter().any(|p| p.ends_with(sid)));
        assert!(!codex.iter().any(|p| p.ends_with(sid)));

        // a sealed-only capture (no raw) keeps historic semantics: entry counts as-is
        assert!(learned_current(
            &ledger_latest(&sessions, Harness::ClaudeCode),
            sid,
            &dir.join("turns.jsonl.zst")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    /// Sealing a resumed session appends a second zstd frame; zstd -d reads the
    /// concatenation back as gen1 + gen2. Requires zstd on PATH (skips otherwise).
    #[tokio::test]
    async fn seal_file_appends_frames_for_resumed_sessions() {
        if !which("zstd") {
            eprintln!("zstd not on PATH; skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("plant-seal-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let raw = root.join("turns.jsonl");
        let dest = seal_sibling(&raw);

        std::fs::write(&raw, "gen1-line\n").unwrap();
        seal_file(&raw).await.expect("first seal");
        assert!(!raw.exists(), "raw removed after seal");
        assert!(dest.is_file());

        // resume: raw reappears with new content only
        std::fs::write(&raw, "gen2-line\n").unwrap();
        seal_file(&raw).await.expect("merge seal");
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
