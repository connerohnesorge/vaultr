//! Local, rebuildable state for session search indexes.

use crate::normalize::{self, Block, Role};
use crate::recon::Recon;
use crate::vault::{self, Session};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use tantivy::schema::{Schema, Value, STORED, STRING, TEXT};
use tantivy::{doc, Index};

pub const SESSION_INDEX_SCHEMA_VERSION: u32 = 2;

/// Curated vault directories that form the non-conversational corpus.
pub const CURATED_DIRECTORIES: &[&str] = &[
    "learnings",
    "decisions",
    "incidents",
    "runbooks",
    "systems",
    "projects",
    "glossary",
    "tickets",
    "alerts",
    "pit",
    "jobs",
];

/// Directories deliberately excluded from curated lexical search.
pub const EXCLUDED_CURATED_DIRECTORIES: &[&str] =
    &["conversations", "teams", "people", "digests", "preferences"];

pub fn curated_root(sessions_root: &std::path::Path) -> anyhow::Result<PathBuf> {
    sessions_root
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("sessions root has no vault parent"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedDocument {
    pub path: PathBuf,
    pub body: String,
}

/// Read every Markdown record within the curated corpus boundary.
pub fn curated_documents(vault_root: &std::path::Path) -> anyhow::Result<Vec<CuratedDocument>> {
    let mut documents = Vec::new();
    for directory in CURATED_DIRECTORIES {
        let root = vault_root.join(directory);
        if !root.exists() {
            continue;
        }
        for entry in ignore::WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(false)
            .git_exclude(false)
            .parents(false)
            .build()
        {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type().is_some_and(|kind| kind.is_file())
                || !is_curated_path(vault_root, path)
            {
                continue;
            }
            documents.push(CuratedDocument {
                path: path.to_path_buf(),
                body: std::fs::read_to_string(path)?,
            });
        }
    }
    documents.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(documents)
}

/// Extract prompt records only for sessions without a readable capture.
pub fn orphan_prompt_documents(
    sessions_root: &std::path::Path,
) -> anyhow::Result<Vec<CuratedDocument>> {
    let vault_root = curated_root(sessions_root)?;
    let mut documents = Vec::new();
    let input = vault_root.join("input");
    if !input.exists() {
        return Ok(documents);
    }
    for entry in ignore::WalkBuilder::new(&input).hidden(false).build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("jsonl")
        {
            continue;
        }
        let mut prompts = Vec::new();
        for line in
            std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(entry.path())?))
        {
            let line = line?;
            let value: serde_json::Value = serde_json::from_str(&line)?;
            let Some(session) = value.get("session").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let capture = sessions_root.join(session.strip_prefix("sessions/").unwrap_or(session));
            let readable = vault::CaptureGenerations::load(&capture)
                .ok()
                .and_then(|generations| {
                    generations.capture_file().map(std::path::Path::to_path_buf)
                })
                .is_some_and(|path| crate::recon::reconstruct(&path).is_ok());
            if !readable {
                if let Some(prompt) = value.get("prompt").and_then(serde_json::Value::as_str) {
                    prompts.push(prompt.to_string());
                }
            }
        }
        if !prompts.is_empty() {
            documents.push(CuratedDocument {
                path: entry.path().to_path_buf(),
                body: prompts.join("\n"),
            });
        }
    }
    documents.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(documents)
}

pub fn is_curated_path(vault_root: &std::path::Path, path: &std::path::Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("md")
        && path
            .strip_prefix(vault_root)
            .ok()
            .and_then(|relative| relative.components().next())
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|directory| CURATED_DIRECTORIES.contains(&directory))
}

/// Persisted alongside the Tantivy session index.
///
/// The index is a cache. A version change makes its documents unreadable until
/// the indexer deletes and rebuilds the directory.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Metadata {
    pub schema_version: u32,
    pub built_at: Option<String>,
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
}

impl Metadata {
    pub fn current() -> Self {
        Self {
            schema_version: SESSION_INDEX_SCHEMA_VERSION,
            ..Self::default()
        }
    }

    pub fn compatible(&self) -> bool {
        self.schema_version == SESSION_INDEX_SCHEMA_VERSION
    }
}

/// Searchable conversation segment beginning with a textual user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedTurn {
    pub turn_index: usize,
    pub body: String,
    pub compacted: bool,
    pub partial: bool,
    pub content_hash: String,
    pub session_id: String,
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub timestamp: Option<String>,
}

/// Convert the complete observed history into search documents.
///
/// A textual user message starts a document. Assistant output and tool I/O stay
/// with that document until the next textual user message. This intentionally
/// uses `observed_messages`, not the final replay, so compaction cannot erase
/// search evidence.
pub fn derive_turns(session: &Session, reconstruction: &Recon) -> Vec<DerivedTurn> {
    let mut turns = Vec::new();
    for observed in &reconstruction.observed_messages {
        for message in normalize::normalize(std::slice::from_ref(&observed.message)) {
            let starts_turn = message.role == Role::User
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, Block::Text(_)));
            if starts_turn {
                turns.push(DerivedTurn {
                    turn_index: turns.len(),
                    body: String::new(),
                    compacted: !observed.in_final_replay,
                    partial: reconstruction.partial,
                    content_hash: String::new(),
                    session_id: session.id.clone(),
                    harness: reconstruction
                        .harness
                        .map(|harness| match harness {
                            crate::recon::Harness::Claude => "claude-code".to_string(),
                            crate::recon::Harness::Codex => "codex".to_string(),
                        })
                        .or_else(|| session.meta.harness.clone()),
                    cwd: session.meta.cwd.clone(),
                    branch: session.meta.git_branch.clone(),
                    timestamp: session
                        .meta
                        .last_observation
                        .clone()
                        .or_else(|| session.meta.original_start.clone()),
                });
            }
            let Some(turn) = turns.last_mut() else {
                continue;
            };
            turn.compacted |= !observed.in_final_replay;
            for block in message.blocks {
                let text = match block {
                    Block::Text(text) => text,
                    Block::ToolResult { content, .. } => limit_tool_output(content),
                    Block::ToolUse { name, input, .. } => format!("{name}\n{input}"),
                    Block::Image => continue,
                };
                if !turn.body.is_empty() {
                    turn.body.push('\n');
                }
                turn.body.push_str(&text);
            }
        }
    }
    turns.retain(|turn| !turn.body.is_empty());
    for (index, turn) in turns.iter_mut().enumerate() {
        turn.turn_index = index;
        turn.content_hash = vault::sha256_hex(turn.body.as_bytes());
    }
    turns
}

/// Bound stored tool output without losing the diagnostic tail.
pub fn limit_tool_output(content: String) -> String {
    const LIMIT: usize = 4_000;
    const HEAD: usize = 3_000;
    const TAIL: usize = 1_000;
    if content.chars().count() <= LIMIT {
        return content;
    }
    let head: String = content.chars().take(HEAD).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(TAIL)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}\n[tool output truncated]\n{tail}")
}

/// Content fingerprint for all readable capture generations in one session.
/// Decide whether one session's indexed documents must be replaced.
pub fn needs_replacement(metadata: &Metadata, session_id: &str, fingerprint: &str) -> bool {
    metadata.sources.get(session_id).map(String::as_str) != Some(fingerprint)
}

/// Record the exact source that produced a session's documents.
pub fn record_source(metadata: &mut Metadata, session_id: String, fingerprint: String) {
    metadata.sources.insert(session_id, fingerprint);
}

/// Decode capture files concurrently, using at most `workers` threads.
pub fn decode_captures(
    captures: Vec<PathBuf>,
    workers: usize,
) -> anyhow::Result<Vec<(PathBuf, Recon)>> {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    let workers = workers.max(1).min(captures.len().max(1));
    let next = AtomicUsize::new(0);
    let decoded = Mutex::new(Vec::with_capacity(captures.len()));
    let failure = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= captures.len() || failure.lock().expect("failure lock").is_some() {
                    return;
                }
                let path = captures[index].clone();
                match crate::recon::reconstruct(&path) {
                    Ok(reconstruction) => decoded
                        .lock()
                        .expect("result lock")
                        .push((path, reconstruction)),
                    Err(error) => *failure.lock().expect("failure lock") = Some(error),
                }
            });
        }
    });
    if let Some(error) = failure.into_inner().expect("failure lock") {
        return Err(error);
    }
    let mut decoded = decoded.into_inner().expect("result lock");
    decoded.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(decoded)
}

pub fn source_fingerprint(directory: &std::path::Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let generations = vault::CaptureGenerations::load(directory)?;
    let mut hash = Sha256::new();
    for path in [
        generations.sealed,
        generations.detached.map(|entry| entry.path),
        generations.raw,
    ]
    .into_iter()
    .flatten()
    {
        hash.update(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .as_bytes(),
        );
        hash.update(vault::sha256_file(&path)?.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

const METADATA_FILE: &str = "metadata.json";

/// Create a compatible index directory, deleting incompatible cache contents.
pub fn prepare_directory(directory: &std::path::Path) -> anyhow::Result<(Metadata, bool)> {
    let metadata_path = directory.join(METADATA_FILE);
    let existing = std::fs::read(&metadata_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Metadata>(&bytes).ok());
    if existing.as_ref().is_some_and(Metadata::compatible) {
        return Ok((existing.expect("checked compatible metadata"), false));
    }
    if directory.exists() {
        std::fs::remove_dir_all(directory)?;
    }
    std::fs::create_dir_all(directory)?;
    let metadata = Metadata::current();
    save_metadata(directory, &metadata)?;
    Ok((metadata, true))
}

pub fn save_metadata(directory: &std::path::Path, metadata: &Metadata) -> anyhow::Result<()> {
    std::fs::write(
        directory.join(METADATA_FILE),
        serde_json::to_vec_pretty(metadata)?,
    )?;
    Ok(())
}

pub struct SessionIndexStats {
    pub sessions: usize,
    pub turns: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResults {
    pub total: usize,
    pub hits: Vec<SearchHit>,
    pub built_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub turn_index: String,
    pub score: f32,
    pub body: String,
    pub harness: String,
    pub cwd: String,
    pub branch: String,
    pub timestamp: String,
    pub compacted: bool,
    pub partial: bool,
    pub duplicates: usize,
}

fn stored(document: &tantivy::TantivyDocument, field: tantivy::schema::Field) -> String {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Search the ready local session index.
const SEARCH_FIELDS: &[&str] = &[
    "session_id",
    "harness",
    "cwd",
    "branch",
    "timestamp",
    "turn_index",
    "compacted",
    "partial",
];

/// Preserve unknown `prefix:value` fragments as literal query text.
pub fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| {
            let Some((prefix, _)) = term.split_once(':') else {
                return term.to_string();
            };
            if SEARCH_FIELDS.contains(&prefix) {
                term.to_string()
            } else {
                format!("\"{}\"", term.replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn search_sessions(query: &str, limit: usize, collapse: bool) -> anyhow::Result<SearchResults> {
    let directory = state_root().join("sessions/tantivy");
    if !directory.exists() {
        anyhow::bail!("no readable session index; run `vaultr session index --update`");
    }
    let index = Index::open_in_dir(directory)?;
    let schema = index.schema();
    let body = schema.get_field("body")?;
    let mut parser = tantivy::query::QueryParser::for_index(&index, vec![body]);
    parser.set_conjunction_by_default();
    let query = parser.parse_query(&normalize_query(query))?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let total = searcher.search(&query, &tantivy::collector::Count)?;
    let top = searcher.search(
        &query,
        &tantivy::collector::TopDocs::with_limit(limit.saturating_mul(100).max(limit)),
    )?;
    let fields = [
        "session_id",
        "turn_index",
        "body",
        "harness",
        "cwd",
        "branch",
        "timestamp",
        "compacted",
        "partial",
        "content_hash",
    ]
    .map(|name| schema.get_field(name).expect("session schema field"));
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut hits: Vec<SearchHit> = Vec::new();
    for (score, address) in top {
        let document: tantivy::TantivyDocument = searcher.doc(address)?;
        let hash = stored(&document, fields[9]);
        if collapse {
            if let Some(index) = seen.get(&hash) {
                hits[*index].duplicates += 1;
                continue;
            }
            seen.insert(hash, hits.len());
        }
        hits.push(SearchHit {
            session_id: stored(&document, fields[0]),
            turn_index: stored(&document, fields[1]),
            body: stored(&document, fields[2]),
            harness: stored(&document, fields[3]),
            cwd: stored(&document, fields[4]),
            branch: stored(&document, fields[5]),
            timestamp: stored(&document, fields[6]),
            compacted: stored(&document, fields[7]) == "true",
            partial: stored(&document, fields[8]) == "true",
            score,
            duplicates: 1,
        });
        if hits.len() >= limit {
            break;
        }
    }
    let metadata = std::fs::read(state_root().join("sessions").join(METADATA_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Metadata>(&bytes).ok());
    Ok(SearchResults {
        total,
        hits,
        built_at: metadata.and_then(|metadata| metadata.built_at),
    })
}

fn session_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("body", TEXT | STORED);
    for field in [
        "session_id",
        "harness",
        "cwd",
        "branch",
        "timestamp",
        "turn_index",
        "compacted",
        "partial",
        "content_hash",
    ] {
        builder.add_text_field(field, STORED | STRING);
    }
    builder.build()
}

/// Rebuild the local lexical session index from every readable local capture.
pub fn build_session_index(
    sessions_root: &std::path::Path,
    workers: usize,
) -> anyhow::Result<SessionIndexStats> {
    let directory = state_root().join("sessions");
    let (mut metadata, _) = prepare_directory(&directory)?;
    let tantivy_directory = directory.join("tantivy");
    let schema = session_schema();
    let index = if tantivy_directory.exists() {
        Index::open_in_dir(&tantivy_directory)?
    } else {
        std::fs::create_dir_all(&tantivy_directory)?;
        Index::create_in_dir(&tantivy_directory, schema.clone())?
    };
    let fields = [
        "body",
        "session_id",
        "harness",
        "cwd",
        "branch",
        "timestamp",
        "turn_index",
        "compacted",
        "partial",
        "content_hash",
    ]
    .map(|name| {
        index
            .schema()
            .get_field(name)
            .expect("session schema field")
    });
    let mut writer = index.writer(50_000_000)?;
    writer.delete_all_documents()?;
    metadata.sources.clear();
    let mut sources = HashMap::new();
    let mut captures = Vec::new();
    for session in vault::list_sessions(sessions_root)? {
        let Some(directory) = vault::find_session_dir(sessions_root, &session)? else {
            continue;
        };
        let generations = vault::CaptureGenerations::load(&directory)?;
        let Some(capture) = generations.capture_file() else {
            continue;
        };
        let capture = capture.to_path_buf();
        let fingerprint = source_fingerprint(&directory)?;
        sources.insert(capture.clone(), (session, fingerprint));
        captures.push(capture);
    }
    let mut stats = SessionIndexStats {
        sessions: 0,
        turns: 0,
    };
    for (capture, reconstruction) in decode_captures(captures, workers)? {
        let (session, fingerprint) = sources.remove(&capture).expect("decoded indexed capture");
        for turn in derive_turns(&session, &reconstruction) {
            writer.add_document(doc!(
                fields[0] => turn.body,
                fields[1] => turn.session_id,
                fields[2] => turn.harness.unwrap_or_default(),
                fields[3] => turn.cwd.unwrap_or_default(),
                fields[4] => turn.branch.unwrap_or_default(),
                fields[5] => turn.timestamp.unwrap_or_default(),
                fields[6] => turn.turn_index.to_string(),
                fields[7] => turn.compacted.to_string(),
                fields[8] => turn.partial.to_string(),
                fields[9] => turn.content_hash,
            ))?;
            stats.turns += 1;
        }
        record_source(&mut metadata, session.id, fingerprint);
        stats.sessions += 1;
    }
    writer.commit()?;
    metadata.built_at = Some(chrono::Utc::now().to_rfc3339());
    save_metadata(&directory, &metadata)?;
    Ok(stats)
}

pub fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"))
        .join("vaultr")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_version_controls_compatibility() {
        assert!(Metadata::current().compatible());
        assert!(!Metadata {
            schema_version: 0,
            ..Metadata::default()
        }
        .compatible());
    }

    #[test]
    fn query_normalization_keeps_unknown_prefixes_literal() {
        assert_eq!(normalize_query("cwd:/work"), "cwd:/work");
        assert_eq!(normalize_query("paste:thing"), "\"paste:thing\"");
        assert_eq!(normalize_query("plain bad:value"), "plain \"bad:value\"");
    }

    #[test]
    fn prompt_sidecars_only_fill_sessions_without_captures() {
        let vault = tempfile::tempdir().unwrap();
        let sessions = vault.path().join("sessions");
        let input = vault.path().join("input/2026/01/01");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&input).unwrap();
        let captured = sessions.join("2026/01/01/captured");
        std::fs::create_dir_all(&captured).unwrap();
        std::fs::write(captured.join("turns.jsonl"), r#"{"harness":"claude-code","request":{"body_delta":{"history":{"key":"messages","prefix_length":0,"append":[]}}}}"#).unwrap();
        std::fs::write(
            input.join("sidecar.jsonl"),
            concat!(
                "{\"session\":\"sessions/2026/01/01/captured\",\"prompt\":\"skip\"}\n",
                "{\"session\":\"sessions/2026/01/01/orphan\",\"prompt\":\"keep\"}\n",
            ),
        )
        .unwrap();
        let documents = orphan_prompt_documents(&sessions).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].body, "keep");
    }

    #[test]
    fn curated_documents_read_only_included_markdown_files() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir(vault.path().join("learnings")).unwrap();
        std::fs::create_dir(vault.path().join("preferences")).unwrap();
        std::fs::write(vault.path().join("learnings/one.md"), "indexed").unwrap();
        std::fs::write(vault.path().join("preferences/one.md"), "excluded").unwrap();
        assert_eq!(
            curated_documents(vault.path()).unwrap(),
            [CuratedDocument {
                path: vault.path().join("learnings/one.md"),
                body: "indexed".to_string(),
            }]
        );
    }

    #[test]
    fn curated_boundary_only_includes_selected_markdown_directories() {
        let root = std::path::Path::new("/vault");
        assert!(is_curated_path(
            root,
            std::path::Path::new("/vault/learnings/one.md")
        ));
        assert!(!is_curated_path(
            root,
            std::path::Path::new("/vault/preferences/one.md")
        ));
        assert!(!is_curated_path(
            root,
            std::path::Path::new("/vault/learnings/one.json")
        ));
    }

    #[test]
    fn incompatible_metadata_rebuilds_the_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("index");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("stale"), "old data").unwrap();
        std::fs::write(directory.join(METADATA_FILE), r#"{"schema_version":0}"#).unwrap();
        let (metadata, rebuilt) = prepare_directory(&directory).unwrap();
        assert!(rebuilt);
        assert!(metadata.compatible());
        assert!(!directory.join("stale").exists());
    }

    #[test]
    fn fingerprints_request_only_changed_session_replacement() {
        let mut metadata = Metadata::current();
        assert!(needs_replacement(&metadata, "one", "first"));
        record_source(&mut metadata, "one".to_string(), "first".to_string());
        assert!(!needs_replacement(&metadata, "one", "first"));
        assert!(needs_replacement(&metadata, "one", "second"));
        assert!(needs_replacement(&metadata, "two", "first"));
    }

    #[test]
    fn tool_output_keeps_the_required_head_and_tail() {
        let input = format!("{}{}", "a".repeat(3_500), "z".repeat(1_500));
        let output = limit_tool_output(input);
        assert!(output.starts_with(&"a".repeat(3_000)));
        assert!(output.ends_with(&"z".repeat(1_000)));
    }

    #[test]
    fn derived_turn_keeps_tool_io_with_the_prompt() {
        let reconstruction = crate::recon::reconstruct_reader(
            &br#"{"harness":"claude-code","request":{"body_delta":{"history":{"key":"messages","prefix_length":0,"append":[{"role":"user","content":"find it"},{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"path":"a"}}]},{"role":"user","content":[{"type":"tool_result","content":"found it"}]}]}}}}"#[..],
        )
        .unwrap();
        let session = Session {
            id: "session-id".to_string(),
            meta: crate::vault::Meta {
                cwd: Some("/work".to_string()),
                git_branch: Some("main".to_string()),
                ..Default::default()
            },
        };
        let turns = derive_turns(&session, &reconstruction);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].body.contains("find it"));
        assert!(turns[0].body.contains("Read"));
        assert!(turns[0].body.contains("found it"));
        assert_eq!(turns[0].session_id, "session-id");
        assert_eq!(turns[0].cwd.as_deref(), Some("/work"));
    }
}
