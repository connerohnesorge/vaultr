//! Local, rebuildable lexical indexes for captured sessions and curated records.

mod store;

use crate::digest::sha256_hex;
use crate::normalize::{self, Block, Role};
use crate::recon::Recon;
use crate::vault::{self, Session};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub use store::{
    normalize_query, prepare_directory, search, update_indexes, Coverage, CuratedHit,
    DuplicateMember, Metadata, SearchHit, SearchOptions, SearchResults, SessionIndexStats,
    SESSION_INDEX_SCHEMA_VERSION,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedDocument {
    pub path: PathBuf,
    pub body: String,
}

/// Read every Markdown record within the curated corpus boundary.
pub fn curated_documents(vault_root: &Path) -> anyhow::Result<Vec<CuratedDocument>> {
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

/// Extract prompt records only for sessions without a local capture.
pub fn orphan_prompt_documents(sessions_root: &Path) -> anyhow::Result<Vec<CuratedDocument>> {
    let readable = readable_session_ids(sessions_root)?;
    orphan_prompt_documents_for(sessions_root, &readable)
}

pub(crate) fn orphan_prompt_documents_for(
    sessions_root: &Path,
    readable: &HashSet<String>,
) -> anyhow::Result<Vec<CuratedDocument>> {
    let vault_root = sessions_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sessions root has no vault parent"))?;
    let input = vault_root.join("input");
    if !input.exists() {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for entry in ignore::WalkBuilder::new(&input)
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .parents(false)
        .build()
    {
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
        for line in BufRead::lines(std::io::BufReader::new(std::fs::File::open(entry.path())?)) {
            let value: Value = serde_json::from_str(&line?)?;
            let Some(session) = value.get("session").and_then(Value::as_str) else {
                continue;
            };
            let session_id = Path::new(session)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(session);
            if readable.contains(session_id) {
                continue;
            }
            if let Some(prompt) = value.get("prompt").and_then(Value::as_str) {
                prompts.push(prompt.to_string());
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

pub fn is_curated_path(vault_root: &Path, path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("md")
        && path
            .strip_prefix(vault_root)
            .ok()
            .and_then(|relative| relative.components().next())
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|directory| CURATED_DIRECTORIES.contains(&directory))
}

fn readable_session_ids(sessions_root: &Path) -> anyhow::Result<HashSet<String>> {
    if !sessions_root.exists() {
        return Ok(HashSet::new());
    }
    Ok(vault::walk_session_dirs(sessions_root)?
        .into_iter()
        .filter_map(|(session_id, directory)| {
            vault::CaptureGenerations::load(&directory)
                .ok()
                .and_then(|generations| {
                    generations
                        .capture_file()
                        .filter(|capture| crate::recon::reconstruct(capture).is_ok())
                        .map(|_| session_id)
                })
        })
        .collect())
}

/// Searchable conversation segment beginning with a textual user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedTurn {
    pub turn_index: usize,
    pub body: String,
    pub literal: String,
    pub compacted: bool,
    pub partial: bool,
    pub content_hash: String,
    pub session_id: String,
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Debug)]
struct TurnBuilder {
    turn: DerivedTurn,
    canonical_blocks: Vec<Value>,
}

/// Convert complete observed history into occurrence-preserving search documents.
pub fn derive_turns(session: &Session, reconstruction: &Recon) -> Vec<DerivedTurn> {
    let mut turns: Vec<TurnBuilder> = Vec::new();
    for observed in &reconstruction.observed_messages {
        for message in normalize::normalize(std::slice::from_ref(&observed.message)) {
            let starts_turn = message.role == Role::User
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, Block::Text(_)));
            if starts_turn {
                turns.push(TurnBuilder {
                    turn: DerivedTurn {
                        turn_index: turns.len(),
                        body: String::new(),
                        literal: String::new(),
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
                        model: session.meta.model.clone(),
                        timestamp: observed
                            .observed_at
                            .clone()
                            .or_else(|| session.meta.last_observation.clone())
                            .or_else(|| session.meta.original_start.clone()),
                    },
                    canonical_blocks: Vec::new(),
                });
            }
            let Some(builder) = turns.last_mut() else {
                continue;
            };
            builder.turn.compacted |= !observed.in_final_replay;
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            for block in message.blocks {
                let (body, literal, canonical) = match block {
                    Block::Text(text) => (
                        Some(text.clone()),
                        identifier_fragments(&text),
                        Some(json!({"role": role, "type": "text", "text": text})),
                    ),
                    Block::ToolResult {
                        content,
                        correlation_id,
                    } => (
                        Some(limit_tool_output(content.clone())),
                        None,
                        Some(json!({
                            "role": role,
                            "type": "tool_result",
                            "content": content,
                            "correlation_id": correlation_id,
                        })),
                    ),
                    Block::ToolUse {
                        name,
                        input,
                        correlation_id,
                    } => {
                        let text = format!("{name}\n{input}");
                        (
                            Some(text.clone()),
                            Some(text),
                            Some(json!({
                                "role": role,
                                "type": "tool_use",
                                "name": name,
                                "input": input,
                                "correlation_id": correlation_id,
                            })),
                        )
                    }
                    Block::Image => (None, None, None),
                };
                if let Some(text) = body {
                    push_segment(&mut builder.turn.body, &text);
                }
                if let Some(text) = literal {
                    push_segment(&mut builder.turn.literal, &text);
                }
                if let Some(canonical) = canonical {
                    builder.canonical_blocks.push(canonical);
                }
            }
        }
    }
    turns
        .into_iter()
        .filter(|builder| !builder.turn.body.is_empty())
        .enumerate()
        .map(|(index, mut builder)| {
            builder.turn.turn_index = index;
            builder.turn.content_hash = sha256_hex(
                &serde_json::to_vec(&builder.canonical_blocks)
                    .expect("canonical normalized blocks serialize"),
            );
            builder.turn
        })
        .collect()
}

fn push_segment(body: &mut String, text: &str) {
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(text);
}

fn identifier_fragments(text: &str) -> Option<String> {
    let fragments = text
        .split_whitespace()
        .filter(|token| {
            token.chars().count() >= 3
                && (token.contains(['/', '\\', '_', '-', '.', ':']) || has_case_transition(token))
        })
        .collect::<Vec<_>>();
    (!fragments.is_empty()).then(|| fragments.join(" "))
}

fn has_case_transition(token: &str) -> bool {
    token
        .chars()
        .zip(token.chars().skip(1))
        .any(|(left, right)| left.is_lowercase() && right.is_uppercase())
}

/// Bound stored tool output without losing the diagnostic head or tail.
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
    format!("{head}{tail}")
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSource {
    pub session: Session,
    pub capture: PathBuf,
    pub fingerprint: String,
}

pub(crate) fn session_sources(sessions_root: &Path) -> anyhow::Result<Vec<SessionSource>> {
    let mut sources = Vec::new();
    for session in vault::list_sessions(sessions_root)? {
        let Some(directory) = vault::find_session_dir(sessions_root, &session)? else {
            continue;
        };
        let generations = vault::CaptureGenerations::load(&directory)?;
        let Some(capture) = generations.capture_file() else {
            continue;
        };
        let mut hash = Sha256::new();
        hash.update(source_fingerprint(&directory)?.as_bytes());
        hash.update(serde_json::to_vec(&session.meta)?);
        sources.push(SessionSource {
            session,
            capture: capture.to_path_buf(),
            fingerprint: format!("{:x}", hash.finalize()),
        });
    }
    sources.sort_by(|left, right| left.session.id.cmp(&right.session.id));
    Ok(sources)
}

/// Cheap source fingerprint for append-only capture generations.
///
/// Raw and sealed captures only append or change generation names. File name,
/// length, and nanosecond modification time therefore detect source changes
/// without rereading the entire vault every five minutes.
pub fn source_fingerprint(directory: &Path) -> anyhow::Result<String> {
    let generations = vault::CaptureGenerations::load(directory)?;
    let mut paths = [
        generations.sealed,
        generations.detached.map(|entry| entry.path),
        generations.raw,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    paths.sort();
    let mut hash = Sha256::new();
    for path in paths {
        let metadata = std::fs::metadata(&path)?;
        hash.update(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .as_bytes(),
        );
        hash.update(metadata.len().to_le_bytes());
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        hash.update(modified.as_nanos().to_le_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
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
    fn prompt_sidecars_only_fill_sessions_without_captures() {
        let vault = tempfile::tempdir().unwrap();
        let sessions = vault.path().join("sessions");
        let input = vault.path().join("input/2026/01/01");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&input).unwrap();
        let captured = sessions.join("2026/01/01/captured");
        std::fs::create_dir_all(&captured).unwrap();
        std::fs::write(captured.join("turns.jsonl"), "{}\n").unwrap();
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
    fn tool_output_keeps_exact_required_head_and_tail() {
        let input = format!("{}{}", "a".repeat(3_500), "z".repeat(1_500));
        let output = limit_tool_output(input);
        assert_eq!(output.chars().count(), 4_000);
        assert!(output.starts_with(&"a".repeat(3_000)));
        assert!(output.ends_with(&"z".repeat(1_000)));
    }

    #[test]
    fn repeated_user_messages_create_distinct_turns() {
        let reconstruction = crate::recon::reconstruct_reader(
            &br#"{"harness":"claude-code","request":{"body_delta":{"history":{"key":"messages","prefix_length":0,"append":[{"role":"user","content":"repeat"}]}}}}
{"harness":"claude-code","request":{"body_delta":{"history":{"key":"messages","prefix_length":1,"append":[{"role":"assistant","content":"first"},{"role":"user","content":"repeat"}]}}}}
"#[..],
        )
        .unwrap();
        let session = Session {
            id: "session-id".to_string(),
            meta: crate::vault::Meta::default(),
        };
        let turns = derive_turns(&session, &reconstruction);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].body, "repeat\nfirst");
        assert_eq!(turns[1].body, "repeat");
        assert_ne!(turns[0].content_hash, turns[1].content_hash);
    }
}
