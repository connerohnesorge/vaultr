//! Session inventory, eligibility policy, and maintenance orchestration.
//! Capture owns scrubbing, detachment, and exact-once Sealing; jobs owns
//! scheduling. Operational Sealing failures remain explicit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::process::{run, run30, which};

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
    for selected in pending_generations(vault)? {
        if !selected.ready_to_seal(&claude, &codex, &jobs, idle) {
            continue;
        }
        let sid = &selected.sid;
        let Some(generation) =
            crate::capture::seal_ready_generation(vault, sid, selected.path().parent().unwrap())
                .await?
        else {
            continue;
        };
        sealed += 1;
        let after = std::fs::metadata(&generation.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if after > COMMIT_CAP {
            exclude_from_commit(vault, &generation.path, after);
        }
        let path = generation.path.display().to_string();
        let relative = path.split("/sessions/").nth(1).unwrap_or(&path);
        println!(
            "[compress] {relative}: {:.1}MB -> {:.1}MB",
            generation.source_len as f64 / 1e6,
            after as f64 / 1e6
        );
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
