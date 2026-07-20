//! Vault root resolution, .meta index reading, session id resolution.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
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
        if let Some(candidate) = existing_session_dir(root, &candidate)? {
            return Ok(candidate);
        }
    }
    // Fallback: scan YYYY/MM/DD for the id (lazy walk, stops at the first hit).
    if let Some((_, dir)) = walk_session_dirs(root)?
        .into_iter()
        .find(|(sid, _)| *sid == session.id)
    {
        return Ok(dir);
    }
    bail!(
        "session directory for {} not found under {}",
        session.id,
        root.display()
    )
}

fn existing_session_dir(root: &Path, candidate: &Path) -> Result<Option<PathBuf>> {
    let relative = candidate
        .strip_prefix(root)
        .with_context(|| format!("session path is not under {}", root.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect session path {}", current.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            bail!("symlinked session path level at {}", current.display());
        }
        if !metadata.is_dir() {
            return Ok(None);
        }
    }
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalize session root {}", root.display()))?;
    let canonical_candidate = std::fs::canonicalize(candidate)
        .with_context(|| format!("canonicalize session {}", candidate.display()))?;
    if !canonical_candidate.starts_with(canonical_root) {
        bail!(
            "session path escapes canonical root at {}",
            candidate.display()
        );
    }
    Ok(Some(candidate.to_path_buf()))
}

#[derive(Clone, Debug)]
pub struct DetachedGeneration {
    pub path: PathBuf,
    pub base_len: u64,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct CaptureGenerations {
    pub sealed: Option<PathBuf>,
    pub detached: Option<DetachedGeneration>,
    pub raw: Option<PathBuf>,
}

pub(crate) enum CaptureGenerationName {
    Raw,
    Sealed,
    Detached { base_len: u64, digest: String },
}

pub(crate) fn parse_capture_generation_name(name: &str) -> Result<Option<CaptureGenerationName>> {
    match name {
        "turns.jsonl" => Ok(Some(CaptureGenerationName::Raw)),
        "turns.jsonl.zst" => Ok(Some(CaptureGenerationName::Sealed)),
        _ => {
            let Some(rest) = name.strip_prefix("turns.jsonl.sealing-") else {
                return Ok(None);
            };
            let (base_len, digest) = rest
                .split_once('-')
                .ok_or_else(|| anyhow::anyhow!("invalid detached generation name"))?;
            let base_len = base_len
                .parse::<u64>()
                .context("invalid detached generation base length")?;
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("invalid detached generation digest");
            }
            Ok(Some(CaptureGenerationName::Detached {
                base_len,
                digest: digest.to_ascii_lowercase(),
            }))
        }
    }
}

impl CaptureGenerations {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut generations = Self {
            sealed: None,
            detached: None,
            raw: None,
        };
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("read session directory {}", dir.display()))?
        {
            let entry =
                entry.with_context(|| format!("read session entry under {}", dir.display()))?;
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            let Some(kind) = parse_capture_generation_name(&name)
                .with_context(|| format!("invalid detached generation at {}", path.display()))?
            else {
                continue;
            };
            if !entry
                .file_type()
                .with_context(|| format!("inspect capture generation {}", path.display()))?
                .is_file()
            {
                bail!(
                    "capture generation is not a regular file at {}",
                    path.display()
                );
            }
            let file = std::fs::File::open(&path)
                .with_context(|| format!("open capture generation {}", path.display()))?;
            match kind {
                CaptureGenerationName::Raw => generations.raw = Some(path),
                CaptureGenerationName::Sealed => generations.sealed = Some(path),
                CaptureGenerationName::Detached { base_len, digest } => {
                    if sha256_reader(file)
                        .with_context(|| format!("hash capture generation {}", path.display()))?
                        != digest
                    {
                        bail!("detached generation digest mismatch at {}", path.display());
                    }
                    if generations
                        .detached
                        .replace(DetachedGeneration {
                            path,
                            base_len,
                            digest: digest.to_ascii_lowercase(),
                        })
                        .is_some()
                    {
                        bail!("multiple detached capture generations in {}", dir.display());
                    }
                }
            }
        }
        Ok(generations)
    }

    pub fn capture_file(&self) -> Option<&Path> {
        self.raw.as_deref().or(self.sealed.as_deref()).or_else(|| {
            self.detached
                .as_ref()
                .map(|generation| generation.path.as_path())
        })
    }

    pub fn unsealed_file(&self) -> Option<&Path> {
        self.detached
            .as_ref()
            .map(|generation| generation.path.as_path())
            .or(self.raw.as_deref())
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(data);
    format!("{:x}", hash.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("hash capture generation {}", path.display()))?;
    sha256_reader(file)
}

pub fn sha256_reader(mut reader: impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Decode the concatenated-zstd suffix at `offset` and hash its exact
/// uncompressed bytes. Reconstruction and Sealing use this same evidence proof.
pub fn decoded_zstd_suffix_digest(mut reader: impl Read + Seek, offset: u64) -> Result<String> {
    reader.seek(SeekFrom::Start(offset))?;
    let decoder = zstd::Decoder::new(reader)?;
    sha256_reader(decoder)
}

/// The capture file inside a session directory.
pub fn capture_file(dir: &Path) -> Result<PathBuf> {
    CaptureGenerations::load(dir)?
        .capture_file()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("no turns.jsonl or turns.jsonl.zst in {}", dir.display()))
}

/// Walk `<root>/YYYY/MM/DD/<session-id>`, yielding `(session_id, session_dir)`
/// in sorted (oldest date first) order. Only all-ASCII-digit directory names count as
/// date levels, so index dirs like `.meta` at the root are never entered.
pub fn walk_session_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalize session root {}", root.display()))?;
    let mut sessions = Vec::new();
    for year in read_dirs(root)? {
        for month in read_dirs(&year)? {
            for day in read_dirs(&month)? {
                // Session ids are UUID-like, so the leaf level takes any dir name.
                for session in read_dirs_where(&day, |_| true)? {
                    let canonical_session = std::fs::canonicalize(&session)
                        .with_context(|| format!("canonicalize session {}", session.display()))?;
                    if !canonical_session.starts_with(&canonical_root) {
                        bail!(
                            "session path escapes canonical root at {}",
                            session.display()
                        );
                    }
                    let Some(sid) = session
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(String::from)
                    else {
                        continue;
                    };
                    sessions.push((sid, session));
                }
            }
        }
    }
    Ok(sessions)
}

/// Sorted subdirectories of `path` whose (UTF-8) name passes `keep`.
fn read_dirs_where(path: &Path, keep: fn(&str) -> bool) -> Result<Vec<PathBuf>> {
    let entries =
        std::fs::read_dir(path).with_context(|| format!("read directory {}", path.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("read directory entry under {}", path.display()))?;
        let entry_path = entry.path();
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if !keep(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspect directory entry {}", entry_path.display()))?;
        if file_type.is_symlink() {
            bail!("symlinked session path level at {}", entry_path.display());
        }
        if file_type.is_dir() {
            dirs.push(entry_path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn read_dirs(path: &Path) -> Result<Vec<PathBuf>> {
    read_dirs_where(path, |n| n.chars().all(|c| c.is_ascii_digit()))
}
