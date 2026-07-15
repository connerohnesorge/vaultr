//! Session-sweep primitives: eligibility discovery, scrub, compress. Orchestration
//! (scheduling, agent panes) lives in jobs.rs; these are exposed as `plant sessions
//! eligible` / `plant compress once` subcommands and composed in jobs/*.md bodies.
//! Every failure path non-fatal: capture uptime is sacred. All heavy work shells out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub struct RunResult {
    pub ok: bool,
    pub out: String,
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
        },
        _ => RunResult {
            ok: false,
            out: String::new(),
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

fn ledger_sessions(vault: &Path, learner: &str) -> HashSet<String> {
    let mut processed = HashSet::new();
    if let Ok(text) =
        std::fs::read_to_string(vault.join("..").join("learnings").join(".ledger.jsonl"))
    {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let recorded = v.get("learner").and_then(|s| s.as_str());
                if recorded == Some(learner) || (recorded.is_none() && learner == "claude") {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        processed.insert(sid.to_string());
                    }
                }
            }
        }
    }
    processed
}

fn capture_files(vault: &Path) -> Vec<(String, PathBuf)> {
    turns_files(vault, true)
}

/// glob */*/*/*/turns.jsonl[.zst] under vault -> (session_id, path)
fn turns_files(vault: &Path, include_compressed: bool) -> Vec<(String, PathBuf)> {
    let mut out = vec![];
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    for y in dirs(vault) {
        for m in dirs(&y) {
            for d in dirs(&m) {
                for sess in dirs(&d) {
                    let raw = sess.join("turns.jsonl");
                    let f = if raw.is_file() {
                        raw
                    } else if include_compressed {
                        sess.join("turns.jsonl.zst")
                    } else {
                        continue;
                    };
                    if f.is_file() {
                        if let Some(sid) = sess.file_name().and_then(|s| s.to_str()) {
                            out.push((sid.to_string(), f));
                        }
                    }
                }
            }
        }
    }
    out
}

fn idle_for(path: &Path, idle: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d >= idle)
        .unwrap_or(false)
}

pub fn eligible_sessions(vault: &Path, idle: Duration, max: usize, learner: &str) -> Vec<String> {
    let processed = ledger_sessions(vault, learner);
    let mut out = vec![];
    for (sid, path) in capture_files(vault) {
        if processed.contains(&sid) || !idle_for(&path, idle) {
            continue;
        }
        let compressed = path.extension().and_then(|e| e.to_str()) == Some("zst");
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // substance gate: >20KB, or >5 turns (only read small files to count)
        // Compressed sessions already cleared the legacy Claude pass before sealing.
        let substantive = compressed
            || size > 20_480
            || std::fs::read_to_string(&path)
                .map(|t| t.trim_end().lines().count() > 5)
                .unwrap_or(false);
        if substantive {
            if let Some(dir) = path.parent() {
                out.push(dir.display().to_string());
            }
        }
    }
    out.truncate(max);
    out
}

/// Diagnostics for the `sessions eligible` subcommand's stderr — stdout must stay
/// clean (it is substituted into an agent prompt by the learn job).
pub fn eligibility_stats(vault: &Path, learner: &str) -> (usize, usize) {
    (
        capture_files(vault).len(),
        ledger_sessions(vault, learner).len(),
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

pub async fn compress_sweep(vault: &Path, idle: Duration) -> bool {
    if !which("zstd") {
        eprintln!("[compress] zstd not on PATH");
        return false;
    }
    let claude = ledger_sessions(vault, "claude");
    let codex = ledger_sessions(vault, "codex");
    let mut sealed = 0u32;
    for (sid, path) in turns_files(vault, false) {
        if !claude.contains(&sid) || !codex.contains(&sid) || !idle_for(&path, idle) {
            continue;
        }
        if !scrub(&path).await {
            continue; // unscrubbed data must not leave the machine
        }
        let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let path_s = path.display().to_string();
        let herdr = path.with_file_name("herdr.jsonl");
        if herdr.is_file() {
            let herdr_s = herdr.display().to_string();
            if !run(
                &["zstd", "-19", "-T0", "-q", "--rm", &herdr_s],
                Duration::from_secs(600),
            )
            .await
            .ok
            {
                continue;
            }
        }
        if run(
            &["zstd", "-19", "-T0", "-q", "--rm", &path_s],
            Duration::from_secs(600),
        )
        .await
        .ok
        {
            sealed += 1;
            let after = std::fs::metadata(format!("{path_s}.zst"))
                .map(|m| m.len())
                .unwrap_or(0);
            let rel = path_s.split("/sessions/").nth(1).unwrap_or(&path_s);
            println!(
                "[compress] {rel}: {:.1}MB -> {:.1}MB",
                before as f64 / 1e6,
                after as f64 / 1e6
            );
        }
    }
    if sealed > 0 {
        let repo = vault.join("..");
        let repo = repo.to_str().unwrap_or(".").to_string();
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

        let claude = eligible_sessions(&sessions, Duration::ZERO, 10, "claude");
        let codex = eligible_sessions(&sessions, Duration::ZERO, 10, "codex");
        assert!(claude.iter().any(|p| p.ends_with(codex_id)));
        assert!(!claude.iter().any(|p| p.ends_with(claude_id)));
        assert!(codex.iter().any(|p| p.ends_with(claude_id)));
        assert!(!codex.iter().any(|p| p.ends_with(codex_id)));

        let _ = std::fs::remove_dir_all(root);
    }
}
