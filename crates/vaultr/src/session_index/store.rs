mod query;

pub use query::{
    normalize_query, search, CuratedHit, DuplicateMember, SearchHit, SearchOptions, SearchResults,
};

use super::{
    curated_documents, derive_turns, orphan_prompt_documents_for, session_sources, state_root,
    CuratedDocument, SessionSource,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, INDEXED, STORED, STRING, TEXT,
};
use tantivy::tokenizer::NgramTokenizer;
use tantivy::{doc, Index, Term};

pub const SESSION_INDEX_SCHEMA_VERSION: u32 = 4;
const METADATA_FILE: &str = "metadata.json";
const NGRAM_TOKENIZER: &str = "vaultr_ngram_3_4";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Coverage {
    pub sessions: usize,
    pub harness: usize,
    pub cwd: usize,
    pub branch: usize,
    pub model: usize,
    pub timestamp: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourceRecord {
    pub fingerprint: String,
    pub documents: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Metadata {
    pub schema_version: u32,
    pub built_at: Option<String>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceRecord>,
    #[serde(default)]
    pub coverage: Coverage,
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

#[derive(Debug, Clone)]
pub struct SessionIndexStats {
    pub sessions: usize,
    pub turns: usize,
    pub changed_sessions: usize,
    pub curated_documents: usize,
    pub changed_curated_documents: usize,
    pub workers: usize,
}

struct UpdateLock {
    file: File,
}

impl UpdateLock {
    fn acquire(root: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join("index.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
        Ok(Self { file })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Update both indexes from one source inventory while holding one writer lock.
pub fn update_indexes(
    sessions_root: &Path,
    workers: usize,
    rebuild: bool,
) -> anyhow::Result<SessionIndexStats> {
    let root = state_root();
    let _lock = UpdateLock::acquire(&root)?;
    let session_directory = root.join("sessions");
    let curated_directory = root.join("curated");
    if rebuild {
        remove_if_exists(&session_directory)?;
        remove_if_exists(&curated_directory)?;
    }

    // Finish all fallible source discovery before publishing either index.
    let sessions = session_sources(sessions_root)?;
    let readable: HashSet<String> = sessions
        .iter()
        .map(|source| source.session.id.clone())
        .collect();
    let vault_root = sessions_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sessions root has no vault parent"))?;
    let curated = curated_sources(vault_root, sessions_root, &readable)?;
    let built_at = chrono::Utc::now().to_rfc3339();

    let session_stats =
        update_session_index(&session_directory, sessions, workers, built_at.clone())?;
    let curated_stats = update_curated_index(&curated_directory, curated, built_at)?;
    Ok(SessionIndexStats {
        sessions: session_stats.sources,
        turns: session_stats.documents,
        changed_sessions: session_stats.changed,
        curated_documents: curated_stats.sources,
        changed_curated_documents: curated_stats.changed,
        workers: workers.max(1),
    })
}

fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct UpdateStats {
    sources: usize,
    documents: usize,
    changed: usize,
}

fn update_session_index(
    directory: &Path,
    sources: Vec<SessionSource>,
    workers: usize,
    built_at: String,
) -> anyhow::Result<UpdateStats> {
    let (mut metadata, rebuilt) = prepare_directory(directory)?;
    let (index, created) = open_or_create_index(directory, session_schema())?;
    if rebuilt || created {
        metadata.sources.clear();
    }
    let fields = SessionFields::load(&index.schema())?;
    let coverage = coverage_from_sources(&sources);
    let fingerprints: BTreeMap<String, String> = sources
        .iter()
        .map(|source| (source.session.id.clone(), source.fingerprint.clone()))
        .collect();
    let changed: Vec<SessionSource> = sources
        .into_iter()
        .filter(|source| {
            metadata
                .sources
                .get(&source.session.id)
                .is_none_or(|record| record.fingerprint != source.fingerprint)
        })
        .collect();
    let removed: Vec<String> = metadata
        .sources
        .keys()
        .filter(|source_id| !fingerprints.contains_key(*source_id))
        .cloned()
        .collect();
    let changed_count = changed.len() + removed.len();
    let mut next_sources = metadata.sources.clone();
    for source_id in &removed {
        next_sources.remove(source_id);
    }

    if changed_count > 0 {
        let mut writer = index.writer(50_000_000)?;
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        for source_id in &removed {
            writer.delete_term(Term::from_field_text(fields.session_id, source_id));
        }
        for source in &changed {
            writer.delete_term(Term::from_field_text(fields.session_id, &source.session.id));
        }
        decode_sessions(changed, workers, |source, reconstruction| {
            let turns = derive_turns(&source.session, &reconstruction);
            let document_count = turns.len();
            for turn in turns {
                let document_id = format!("{}:{}", turn.session_id, turn.content_hash);
                writer.add_document(doc!(
                    fields.body => turn.body,
                    fields.literal => turn.literal,
                    fields.document_id => document_id,
                    fields.session_id => turn.session_id,
                    fields.harness => turn.harness.unwrap_or_default(),
                    fields.cwd => turn.cwd.unwrap_or_default(),
                    fields.branch => turn.branch.unwrap_or_default(),
                    fields.model => turn.model.unwrap_or_default(),
                    fields.timestamp => turn.timestamp.unwrap_or_default(),
                    fields.turn_index => turn.turn_index as u64,
                    fields.compacted => turn.compacted,
                    fields.partial => turn.partial,
                    fields.content_hash => turn.content_hash,
                ))?;
            }
            next_sources.insert(
                source.session.id,
                SourceRecord {
                    fingerprint: source.fingerprint,
                    documents: document_count,
                },
            );
            Ok(())
        })?;
        writer.commit()?;
    }

    metadata.sources = next_sources;
    metadata.built_at = Some(built_at);
    metadata.coverage = coverage;
    save_metadata(directory, &metadata)?;
    Ok(UpdateStats {
        sources: metadata.sources.len(),
        documents: metadata
            .sources
            .values()
            .map(|record| record.documents)
            .sum(),
        changed: changed_count,
    })
}

fn coverage_from_sources(sources: &[SessionSource]) -> Coverage {
    Coverage {
        sessions: sources.len(),
        harness: sources
            .iter()
            .filter(|source| source.session.meta.harness.is_some())
            .count(),
        cwd: sources
            .iter()
            .filter(|source| source.session.meta.cwd.is_some())
            .count(),
        branch: sources
            .iter()
            .filter(|source| source.session.meta.git_branch.is_some())
            .count(),
        model: sources
            .iter()
            .filter(|source| source.session.meta.model.is_some())
            .count(),
        timestamp: sources
            .iter()
            .filter(|source| {
                source.session.meta.last_observation.is_some()
                    || source.session.meta.original_start.is_some()
            })
            .count(),
    }
}

#[derive(Debug, Clone)]
struct CuratedSource {
    key: String,
    path: String,
    body: String,
    fingerprint: String,
}

fn curated_sources(
    vault_root: &Path,
    sessions_root: &Path,
    readable: &HashSet<String>,
) -> anyhow::Result<Vec<CuratedSource>> {
    let mut documents = curated_documents(vault_root)?;
    documents.extend(orphan_prompt_documents_for(sessions_root, readable)?);
    documents.sort_by(|left, right| left.path.cmp(&right.path));
    documents
        .into_iter()
        .map(|document| curated_source(vault_root, document))
        .collect()
}

fn curated_source(vault_root: &Path, document: CuratedDocument) -> anyhow::Result<CuratedSource> {
    let key = document
        .path
        .strip_prefix(vault_root)
        .unwrap_or(&document.path)
        .to_string_lossy()
        .to_string();
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    hash.update(document.body.as_bytes());
    Ok(CuratedSource {
        path: key.clone(),
        key,
        body: document.body,
        fingerprint: format!("{:x}", hash.finalize()),
    })
}

fn update_curated_index(
    directory: &Path,
    sources: Vec<CuratedSource>,
    built_at: String,
) -> anyhow::Result<UpdateStats> {
    let (mut metadata, rebuilt) = prepare_directory(directory)?;
    let (index, created) = open_or_create_index(directory, curated_schema())?;
    if rebuilt || created {
        metadata.sources.clear();
    }
    let schema = index.schema();
    let body = schema.get_field("body")?;
    let literal = schema.get_field("literal")?;
    let source_id = schema.get_field("source_id")?;
    let path = schema.get_field("path")?;
    let fingerprints: BTreeMap<String, String> = sources
        .iter()
        .map(|source| (source.key.clone(), source.fingerprint.clone()))
        .collect();
    let changed: Vec<CuratedSource> = sources
        .into_iter()
        .filter(|source| {
            metadata
                .sources
                .get(&source.key)
                .is_none_or(|record| record.fingerprint != source.fingerprint)
        })
        .collect();
    let removed: Vec<String> = metadata
        .sources
        .keys()
        .filter(|key| !fingerprints.contains_key(*key))
        .cloned()
        .collect();
    let changed_count = changed.len() + removed.len();
    let mut next_sources = metadata.sources.clone();
    if changed_count > 0 {
        let mut writer = index.writer(50_000_000)?;
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        for key in &removed {
            writer.delete_term(Term::from_field_text(source_id, key));
            next_sources.remove(key);
        }
        for source in changed {
            writer.delete_term(Term::from_field_text(source_id, &source.key));
            writer.add_document(doc!(
                body => source.body.clone(),
                literal => format!("{}\n{}", source.path, source.body),
                source_id => source.key.clone(),
                path => source.path,
            ))?;
            next_sources.insert(
                source.key,
                SourceRecord {
                    fingerprint: source.fingerprint,
                    documents: 1,
                },
            );
        }
        writer.commit()?;
    }
    metadata.sources = next_sources;
    metadata.built_at = Some(built_at);
    save_metadata(directory, &metadata)?;
    Ok(UpdateStats {
        sources: metadata.sources.len(),
        documents: metadata
            .sources
            .values()
            .map(|record| record.documents)
            .sum(),
        changed: changed_count,
    })
}

fn decode_sessions(
    sources: Vec<SessionSource>,
    workers: usize,
    mut visit: impl FnMut(SessionSource, crate::recon::Recon) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if sources.is_empty() {
        return Ok(());
    }
    let worker_count = workers.max(1).min(sources.len());
    let next = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let (sender, receiver) = mpsc::sync_channel(worker_count.saturating_mul(2).max(1));
    let mut first_error = None;
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let sources = &sources;
            let next = &next;
            let cancelled = &cancelled;
            scope.spawn(move || loop {
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(source) = sources.get(index).cloned() else {
                    break;
                };
                let result = crate::recon::reconstruct(&source.capture)
                    .map(|reconstruction| (source, reconstruction));
                if result.is_err() {
                    cancelled.store(true, Ordering::Release);
                }
                if sender.send(result).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        for result in receiver {
            if first_error.is_some() {
                continue;
            }
            match result {
                Ok((source, reconstruction)) => {
                    if let Err(error) = visit(source, reconstruction) {
                        cancelled.store(true, Ordering::Release);
                        first_error = Some(error);
                    }
                }
                Err(error) => first_error = Some(error),
            }
        }
    });
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Create a compatible corpus directory, deleting incompatible cache contents.
pub fn prepare_directory(directory: &Path) -> anyhow::Result<(Metadata, bool)> {
    let metadata_path = directory.join(METADATA_FILE);
    let existing = std::fs::read(&metadata_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Metadata>(&bytes).ok());
    if existing.as_ref().is_some_and(Metadata::compatible) {
        return Ok((existing.expect("compatible metadata"), false));
    }
    remove_if_exists(directory)?;
    std::fs::create_dir_all(directory)?;
    let metadata = Metadata::current();
    save_metadata(directory, &metadata)?;
    Ok((metadata, true))
}

fn save_metadata(directory: &Path, metadata: &Metadata) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(METADATA_FILE);
    let temporary = directory.join(format!(
        ".{METADATA_FILE}.tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> anyhow::Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(metadata)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn open_or_create_index(directory: &Path, schema: Schema) -> anyhow::Result<(Index, bool)> {
    let tantivy_directory = directory.join("tantivy");
    let (index, created) = if tantivy_directory.exists() {
        (Index::open_in_dir(&tantivy_directory)?, false)
    } else {
        std::fs::create_dir_all(&tantivy_directory)?;
        (Index::create_in_dir(&tantivy_directory, schema)?, true)
    };
    register_tokenizers(&index)?;
    Ok((index, created))
}

fn open_ready_index(directory: &Path) -> anyhow::Result<(Index, Metadata)> {
    let metadata = std::fs::read(directory.join(METADATA_FILE))
        .map_err(|_| anyhow::anyhow!("no readable index metadata"))
        .and_then(|bytes| Ok(serde_json::from_slice::<Metadata>(&bytes)?))?;
    if !metadata.compatible() {
        anyhow::bail!("index schema is incompatible");
    }
    let index = Index::open_in_dir(directory.join("tantivy"))?;
    register_tokenizers(&index)?;
    Ok((index, metadata))
}

fn register_tokenizers(index: &Index) -> anyhow::Result<()> {
    index
        .tokenizers()
        .register(NGRAM_TOKENIZER, NgramTokenizer::new(3, 4, false)?);
    Ok(())
}

fn literal_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(NGRAM_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

fn session_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("body", TEXT | STORED);
    builder.add_text_field("literal", literal_options());
    for field in [
        "document_id",
        "session_id",
        "harness",
        "cwd",
        "branch",
        "model",
        "timestamp",
        "content_hash",
    ] {
        builder.add_text_field(field, STORED | STRING);
    }
    builder.add_u64_field("turn_index", INDEXED | STORED);
    builder.add_bool_field("compacted", INDEXED | STORED);
    builder.add_bool_field("partial", INDEXED | STORED);
    builder.build()
}

fn curated_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("body", TEXT | STORED);
    builder.add_text_field("literal", literal_options());
    builder.add_text_field("source_id", STRING);
    builder.add_text_field("path", STRING | STORED);
    builder.build()
}

struct SessionFields {
    body: Field,
    literal: Field,
    document_id: Field,
    session_id: Field,
    harness: Field,
    cwd: Field,
    branch: Field,
    model: Field,
    timestamp: Field,
    turn_index: Field,
    compacted: Field,
    partial: Field,
    content_hash: Field,
}

impl SessionFields {
    fn load(schema: &Schema) -> tantivy::Result<Self> {
        Ok(Self {
            body: schema.get_field("body")?,
            literal: schema.get_field("literal")?,
            document_id: schema.get_field("document_id")?,
            session_id: schema.get_field("session_id")?,
            harness: schema.get_field("harness")?,
            cwd: schema.get_field("cwd")?,
            branch: schema.get_field("branch")?,
            model: schema.get_field("model")?,
            timestamp: schema.get_field("timestamp")?,
            turn_index: schema.get_field("turn_index")?,
            compacted: schema.get_field("compacted")?,
            partial: schema.get_field("partial")?,
            content_hash: schema.get_field("content_hash")?,
        })
    }
}

#[cfg(test)]
mod tests;
