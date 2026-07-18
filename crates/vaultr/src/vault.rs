//! Vault root resolution, .meta index reading, session id resolution.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Per-session index entry from `<root>/.meta/<session-uuid>.json`.
/// All fields lenient — some entries omit or null them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Meta {
    #[serde(default)]
    pub schema_version: Option<u64>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_start_source: Option<String>,
    #[serde(default)]
    pub original_start: Option<String>,
    #[serde(default)]
    pub last_observation: Option<String>,
}

/// Derive `<root>/YYYY/MM/DD/<session_id>` from an RFC 3339 timestamp in UTC.
pub fn dated_session_dir(
    root: &Path,
    session_id: &str,
    original_start: Option<&str>,
) -> Option<PathBuf> {
    let date = chrono::DateTime::parse_from_rfc3339(original_start?)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(
        root.join(date.format("%Y").to_string())
            .join(date.format("%m").to_string())
            .join(date.format("%d").to_string())
            .join(session_id),
    )
}

impl Meta {
    /// Sort key: most recent activity (ISO strings sort lexicographically).
    pub fn last_activity(&self) -> &str {
        self.last_observation
            .as_deref()
            .or(self.original_start.as_deref())
            .unwrap_or("")
    }
}

/// A session known to the index: id (from the .meta filename) + parsed meta.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub meta: Meta,
}

/// Resolve the sessions root: explicit override > $VAULT_SESSIONS > ~/.dotfiles/vault/sessions.
pub fn root(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    if let Ok(v) = std::env::var("VAULT_SESSIONS") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".dotfiles/vault/sessions"))
}

/// Read all sessions from `<root>/.meta/*.json`, newest-first.
pub fn list_sessions(root: &Path) -> Result<Vec<Session>> {
    let meta_dir = root.join(".meta");
    let entries = std::fs::read_dir(&meta_dir)
        .with_context(|| format!("no session index at {}", meta_dir.display()))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let meta: Meta = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        out.push(Session { id, meta });
    }
    out.sort_by(|a, b| b.meta.last_activity().cmp(a.meta.last_activity()));
    Ok(out)
}

/// Resolve a full UUID or unambiguous prefix against the .meta index.
pub fn resolve_id(root: &Path, query: &str) -> Result<Session> {
    let sessions = list_sessions(root)?;
    if let Some(s) = sessions.iter().find(|s| s.id == query) {
        return Ok(s.clone());
    }
    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.id.starts_with(query))
        .collect();
    match matches.len() {
        0 => bail!("no session matching '{query}'"),
        1 => Ok(matches[0].clone()),
        _ => {
            let list: Vec<String> = matches.iter().map(|s| s.id.clone()).collect();
            bail!(
                "ambiguous session id '{query}', candidates:\n  {}",
                list.join("\n  ")
            )
        }
    }
}

/// Locate the session directory `<root>/YYYY/MM/DD/<id>`.
/// Tries the date derived from original_start first, then scans.
pub fn session_dir(root: &Path, session: &Session) -> Result<PathBuf> {
    if let Some(candidate) =
        dated_session_dir(root, &session.id, session.meta.original_start.as_deref())
    {
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    // Fallback: scan YYYY/MM/DD for the id (lazy walk, stops at the first hit).
    if let Some((_, dir)) = walk_session_dirs(root).find(|(sid, _)| *sid == session.id) {
        return Ok(dir);
    }
    bail!(
        "session directory for {} not found under {}",
        session.id,
        root.display()
    )
}

/// The capture file inside a session dir: raw `turns.jsonl` or `.zst`.
pub fn capture_file(dir: &Path) -> Result<PathBuf> {
    let raw = dir.join("turns.jsonl");
    if raw.is_file() {
        return Ok(raw);
    }
    let zst = dir.join("turns.jsonl.zst");
    if zst.is_file() {
        return Ok(zst);
    }
    bail!("no turns.jsonl or turns.jsonl.zst in {}", dir.display())
}

/// Walk `<root>/YYYY/MM/DD/<session-id>` lazily, yielding `(session_id, session_dir)`
/// in sorted (oldest date first) order. Only all-ASCII-digit directory names count as
/// date levels, so index dirs like `.meta` at the root are never entered. Unreadable
/// dirs are skipped.
pub fn walk_session_dirs(root: &Path) -> impl Iterator<Item = (String, PathBuf)> {
    read_dirs(root).into_iter().flat_map(|y| {
        read_dirs(&y).into_iter().flat_map(|m| {
            read_dirs(&m).into_iter().flat_map(|d| {
                // Session ids are UUID-like, so the leaf level takes any dir name.
                read_dirs_where(&d, |_| true)
                    .into_iter()
                    .filter_map(|sess| {
                        let sid = sess.file_name()?.to_str()?.to_string();
                        Some((sid, sess))
                    })
            })
        })
    })
}

/// Sorted subdirectories of `path` whose (UTF-8) name passes `keep`.
fn read_dirs_where(path: &Path, keep: fn(&str) -> bool) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(path)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(keep))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn read_dirs(path: &Path) -> Vec<PathBuf> {
    read_dirs_where(path, |n| n.chars().all(|c| c.is_ascii_digit()))
}
