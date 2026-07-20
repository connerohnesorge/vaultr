//! Session-sweep primitives: eligibility discovery, scrub, compress. Orchestration
//! (scheduling, agent panes) lives in jobs.rs; these are exposed as `plant sessions
//! eligible` / `plant compress once` subcommands and called directly by the built-in Rust jobs.
//! Every failure path non-fatal: capture uptime is sacred. All heavy work shells out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// The sealed sibling (`turns.jsonl` -> `turns.jsonl.zst`) of a raw capture file.
fn seal_sibling(raw: &Path) -> PathBuf {
    let name = raw.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name.starts_with("turns.jsonl.sealing-") {
        raw.with_file_name("turns.jsonl.zst")
    } else {
        raw.with_file_name(format!("{name}.zst"))
    }
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

fn capture_files(vault: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    turns_files(vault, true)
}

/// YYYY/MM/DD/<id>/turns.jsonl[.zst] under vault -> (session_id, path).
/// Uses the shared digit-filtered walker, so non-date dirs (e.g. `.meta`) are skipped.
fn turns_files(vault: &Path, include_compressed: bool) -> Result<Vec<(String, PathBuf)>, String> {
    let mut out = vec![];
    for (sid, sess) in vaultr::vault::walk_session_dirs(vault).map_err(|e| e.to_string())? {
        let generations =
            vaultr::vault::CaptureGenerations::load(&sess).map_err(|e| e.to_string())?;
        let f = if include_compressed {
            generations.capture_file()
        } else {
            generations.unsealed_file()
        };
        if let Some(f) = f {
            out.push((sid, f.to_path_buf()));
        }
    }
    Ok(out)
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
    for (sid, path) in turns_files(vault, false)? {
        let Some(idle) = idle_secs(&path) else {
            continue;
        };
        if idle < age.as_secs() {
            continue;
        }
        let state = if jobs.contains(&sid) {
            "job-capture".to_string() // plant's own agent pane; informational like sub-threshold
        } else {
            match (
                learned_current(&claude, &sid, &path),
                learned_current(&codex, &sid, &path),
            ) {
                (true, true) => "seal-blocked".to_string(),
                (true, false) => "half-learned:codex".to_string(),
                (false, true) => "half-learned:claude".to_string(),
                (false, false) if substantive(&path) => "unlearned".to_string(),
                (false, false) => "sub-threshold".to_string(),
            }
        };
        out.push(StuckCapture {
            sid,
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
    for (sid, path) in capture_files(vault)? {
        if jobs.contains(&sid)
            || learned_current(&processed, &sid, &path)
            || inflight.contains(&sid)
            || !idle_for(&path, idle)
        {
            continue;
        }
        if substantive(&path) {
            if let Some(dir) = path.parent() {
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
        capture_files(vault)?.len(),
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

fn suffix_matches(path: &Path, offset: u64, suffix: &Path) -> Result<bool, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    let mut expected =
        std::fs::File::open(suffix).map_err(|e| format!("open {}: {e}", suffix.display()))?;
    let mut left = [0u8; 64 * 1024];
    let mut right = [0u8; 64 * 1024];
    loop {
        let a = file
            .read(&mut left)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let b = expected
            .read(&mut right)
            .map_err(|e| format!("read {}: {e}", suffix.display()))?;
        if a != b || left[..a] != right[..b] {
            return Ok(false);
        }
        if a == 0 {
            return Ok(true);
        }
    }
}

/// Commit one immutable detached generation exactly once. The filename records
/// the sealed destination length at detach time; a retry after the destination
/// rename verifies the regenerated frame at that exact offset before cleanup.
async fn seal_detached(generation: &vaultr::vault::DetachedGeneration) -> Result<PathBuf, String> {
    let raw_mtime = std::fs::metadata(&generation.path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", generation.path.display()))?;
    let dest = generation.path.with_file_name("turns.jsonl.zst");
    let frame = generation.path.with_file_name("turns.jsonl.frame-tmp");
    let raw_s = generation.path.display().to_string();
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
    let frame_len = std::fs::metadata(&frame)
        .map(|meta| meta.len())
        .map_err(|e| format!("stat {}: {e}", frame.display()))?;
    let dest_len = match std::fs::metadata(&dest) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("stat {}: {e}", dest.display())),
    };

    let result = if dest_len == generation.base_len {
        let merged = generation.path.with_file_name("turns.jsonl.zst-tmp");
        let assembled = (|| -> std::io::Result<()> {
            let mut out = std::fs::File::create(&merged)?;
            if generation.base_len > 0 {
                std::io::copy(&mut std::fs::File::open(&dest)?, &mut out)?;
            }
            std::io::copy(&mut std::fs::File::open(&frame)?, &mut out)?;
            out.sync_all()?;
            std::fs::rename(&merged, &dest)
        })();
        if let Err(e) = assembled {
            let _ = std::fs::remove_file(&merged);
            Err(e.to_string())
        } else {
            Ok(())
        }
    } else if dest_len == generation.base_len + frame_len
        && suffix_matches(&dest, generation.base_len, &frame)?
    {
        Ok(())
    } else {
        Err(format!(
            "sealed destination conflicts with detached generation at {}",
            dest.display()
        ))
    };
    let _ = std::fs::remove_file(&frame);
    result?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&dest)
        .and_then(|file| file.set_modified(raw_mtime))
        .map_err(|e| format!("set mtime {}: {e}", dest.display()))?;
    std::fs::remove_file(&generation.path).map_err(|e| {
        format!(
            "remove detached generation {}: {e}",
            generation.path.display()
        )
    })?;
    Ok(dest)
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
    let mut sealed = 0u32;
    for (sid, path) in turns_files(vault, false)? {
        let detached = path.file_name().and_then(|name| name.to_str()) != Some("turns.jsonl");
        // job self-captures seal without waiting for learn — they are never dispatched
        let learned = learned_current(&claude, &sid, &path) && learned_current(&codex, &sid, &path);
        if !detached && (!(learned || jobs.contains(&sid)) || !idle_for(&path, idle)) {
            continue;
        }
        let Some(generation) =
            crate::capture::detach_generation(vault, &sid, path.parent().unwrap()).await?
        else {
            continue;
        };
        let before = std::fs::metadata(&generation.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let herdr = generation.path.with_file_name("herdr.jsonl");
        if herdr.is_file() {
            if let Err(e) = seal_file(&herdr).await {
                return Err(format!("seal {sid} herdr.jsonl: {e}"));
            }
        }
        match seal_detached(&generation).await {
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

/// Read a capture file's envelope lines, decompressing `.zst` transparently.
/// ponytail: single-file only — the transient sealed+raw sibling pair (mid-resume)
/// is chained by recon::reconstruct elsewhere; coverage is a point-in-time audit and
/// reads whichever file `capture_file` returns. Unreadable => empty, never fatal.
fn capture_lines(path: &Path) -> Vec<String> {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return vec![];
    };
    let mut text = String::new();
    let ok = if path.extension().and_then(|e| e.to_str()) == Some("zst") {
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
    let cap = vaultr::vault::capture_file(&dir).map_err(|e| e.to_string())?;

    // Captured side: distinct response request-ids, and the window start (min observed_at).
    let mut captured: HashSet<String> = HashSet::new();
    let mut window_start: Option<String> = None;
    for line in capture_lines(&cap) {
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

        let raw = turns_files(&sessions, false).unwrap();
        assert_eq!(raw.len(), 1, "only the dated session, got {raw:?}");
        assert_eq!(raw[0].0, sid);
        assert_eq!(raw[0].1, dated.join("turns.jsonl"));

        // include_compressed also picks up sealed sessions, still date-filtered.
        std::fs::remove_file(dated.join("turns.jsonl")).unwrap();
        std::fs::write(dated.join("turns.jsonl.zst"), "sealed").unwrap();
        let all = turns_files(&sessions, true).unwrap();
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

        // a sealed-only capture (no raw) keeps historic semantics: entry counts as-is
        assert!(learned_current(
            &ledger_latest(&sessions, "claude"),
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

    #[tokio::test]
    async fn detached_generation_retry_after_destination_rename_is_exactly_once() {
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

        std::fs::write(dir.join("turns.jsonl"), "generation-one\n").unwrap();
        let first = crate::capture::detach_generation(&vault, sid, &dir)
            .await
            .unwrap()
            .unwrap();
        seal_detached(&first).await.unwrap();

        std::fs::write(dir.join("turns.jsonl"), "generation-two\n").unwrap();
        let second = crate::capture::detach_generation(&vault, sid, &dir)
            .await
            .unwrap()
            .unwrap();
        let dest = dir.join("turns.jsonl.zst");
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

        seal_detached(&second).await.unwrap();
        assert!(!second.path.exists(), "committed transaction cleaned");
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
