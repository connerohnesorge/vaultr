use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::session_fs::SessionDirectory;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CaptureOrder {
    next_sequence: u64,
    next_to_drain: u64,
    pending: BTreeMap<u64, Value>,
    root: String,
}

impl CaptureOrder {
    fn new(root: &str) -> Self {
        Self {
            next_sequence: 0,
            next_to_drain: 0,
            pending: BTreeMap::new(),
            root: root.to_string(),
        }
    }
}

struct Journal {
    dir: PathBuf,
    state: Map<String, Value>,
    order: Option<CaptureOrder>,
    existed: bool,
}

impl Journal {
    fn load(dir: &Path, sid: &str) -> Result<Self, String> {
        let path = dir.join("state.json");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    dir: dir.to_path_buf(),
                    state: Map::new(),
                    order: None,
                    existed: false,
                });
            }
            Err(error) => {
                return Err(format!("capture journal: read {}: {error}", path.display()));
            }
        };
        let state = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(state)) => state,
            Ok(_) => {
                return Err(format!(
                    "capture journal: {} is not a JSON object",
                    path.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "capture journal: parse {}: {error}",
                    path.display()
                ));
            }
        };
        validate_state(&state, &path, sid)?;
        let order = state
            .get("capture_order")
            .map(|value| {
                serde_json::from_value::<CaptureOrder>(value.clone()).map_err(|error| {
                    format!(
                        "capture journal: invalid capture_order in {}: {error}",
                        path.display()
                    )
                })
            })
            .transpose()?;
        if let Some(order) = &order {
            validate_order(order, &path, sid)?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            state,
            order,
            existed: true,
        })
    }

    fn prior_body(&self) -> Value {
        self.state
            .get("request_body")
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn reserve(&mut self, root: &str, request: Value) -> Result<u64, String> {
        let order = self.order.get_or_insert_with(|| CaptureOrder::new(root));
        if order.root != root {
            return Err(format!(
                "capture journal: vault identity mismatch at {}",
                self.dir.join("state.json").display()
            ));
        }
        let sequence = order.next_sequence;
        order.next_sequence = order.next_sequence.checked_add(1).ok_or_else(|| {
            format!(
                "capture journal: sequence exhausted at {}",
                self.dir.join("state.json").display()
            )
        })?;
        order.pending.insert(sequence, request);
        Ok(sequence)
    }

    fn replace_state(&mut self, state: Map<String, Value>) {
        self.state = state;
    }

    fn persist(&mut self) -> Result<(), String> {
        let order = self.order.as_ref().ok_or_else(|| {
            format!(
                "capture journal: missing capture_order at {}",
                self.dir.join("state.json").display()
            )
        })?;
        self.state.insert(
            "capture_order".into(),
            serde_json::to_value(order).map_err(|error| error.to_string())?,
        );
        let body =
            serde_json::to_vec(&Value::Object(self.state.clone())).map_err(|e| e.to_string())?;
        let mut line = body;
        line.push(b'\n');
        atomic_write(&self.dir.join("state.json"), &line).map_err(|error| {
            format!(
                "capture journal: persist {}: {error}",
                self.dir.join("state.json").display()
            )
        })?;
        self.existed = true;
        Ok(())
    }

    fn require_order(&self) -> Result<&CaptureOrder, String> {
        self.order.as_ref().ok_or_else(|| {
            format!(
                "capture journal: no capture_order in {}",
                self.dir.join("state.json").display()
            )
        })
    }

    fn require_order_mut(&mut self) -> Result<&mut CaptureOrder, String> {
        self.order.as_mut().ok_or_else(|| {
            format!(
                "capture journal: no capture_order in {}",
                self.dir.join("state.json").display()
            )
        })
    }
}

fn validate_state(state: &Map<String, Value>, path: &Path, sid: &str) -> Result<(), String> {
    if state.get("schema_version").and_then(Value::as_u64) != Some(1)
        || state.get("harness").and_then(Value::as_str).is_none()
        || state.get("session_id").and_then(Value::as_str) != Some(sid)
        || !state.contains_key("request_body")
    {
        return Err(format!(
            "capture journal: invalid legacy state at {}",
            path.display()
        ));
    }
    if state
        .get("thread_id")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err(format!(
            "capture journal: invalid thread identity at {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_order(order: &CaptureOrder, path: &Path, sid: &str) -> Result<(), String> {
    if order.root.is_empty() || order.next_to_drain > order.next_sequence {
        return Err(format!(
            "capture journal: invalid ordering bounds at {}",
            path.display()
        ));
    }
    if order
        .pending
        .keys()
        .any(|sequence| *sequence < order.next_to_drain || *sequence >= order.next_sequence)
    {
        return Err(format!(
            "capture journal: pending sequence outside bounds at {}",
            path.display()
        ));
    }
    for sequence in order.next_to_drain..order.next_sequence {
        let Some(request) = order.pending.get(&sequence) else {
            return Err(format!(
                "capture journal: missing request sequence {sequence} at {}",
                path.display()
            ));
        };
        let request_id = request.get("request_id").and_then(Value::as_str);
        if request.as_object().is_none()
            || request.get("session_id").and_then(Value::as_str) != Some(sid)
            || request_id.is_none_or(|id| uuid::Uuid::parse_str(id).is_err())
        {
            return Err(format!(
                "capture journal: invalid request identity at {}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    fs::File::create(&temporary)?.write_all(bytes)?;
    fs::rename(&temporary, path)
}

static SESSION_LOCKS: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    Mutex::new(None);

pub(crate) fn session_lock(root: &str, sid: &str) -> Arc<tokio::sync::Mutex<()>> {
    let key = format!("{root}\0{sid}");
    SESSION_LOCKS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

pub(crate) fn canonical_root(vault: &Path) -> String {
    fs::canonicalize(vault)
        .unwrap_or_else(|_| vault.to_path_buf())
        .display()
        .to_string()
}

pub(super) fn staging_base() -> PathBuf {
    crate::jobs::state_dir().join("capture-staging")
}

pub(super) fn staging_dir(root: &str, sid: &str) -> PathBuf {
    staging_base()
        .join(vaultr::vault::sha256_hex(root.as_bytes()))
        .join(sid)
}

#[derive(Clone)]
struct Stage {
    path: PathBuf,
    sequence: u64,
    envelope: Value,
}

impl Stage {
    fn publish(root: &str, sid: &str, sequence: u64, envelope: Value) -> Result<Self, String> {
        let request_id = envelope
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "capture stage: envelope has no request identity".to_string())?;
        let path = staging_dir(root, sid).join(format!("{sequence}-{request_id}.json"));
        let document = json!({
            "root": root,
            "sequence": sequence,
            "request_id": request_id,
            "envelope": envelope,
        });
        atomic_write(
            &path,
            &serde_json::to_vec(&document).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("capture stage: write {}: {error}", path.display()))?;
        Ok(Self {
            path,
            sequence,
            envelope,
        })
    }
}

fn is_atomic_stage_temp(name: &str) -> bool {
    let Some((stage, temporary_id)) = name.rsplit_once(".tmp-") else {
        return false;
    };
    let Some((sequence, request_id)) = stage.split_once('-') else {
        return false;
    };
    let valid_uuid = |value: &str| {
        uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.hyphenated().to_string() == value)
    };
    sequence
        .parse::<u64>()
        .is_ok_and(|parsed| parsed.to_string() == sequence)
        && valid_uuid(request_id)
        && valid_uuid(temporary_id)
        && uuid::Uuid::parse_str(temporary_id).is_ok_and(|parsed| parsed.get_version_num() == 4)
}

fn read_stages(
    root: &str,
    sid: &str,
    journal: &Journal,
    recover_atomic_temps: bool,
) -> Result<BTreeMap<u64, Stage>, String> {
    let directory = staging_dir(root, sid);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => {
            return Err(format!(
                "capture stage: read directory {}: {error}",
                directory.display()
            ));
        }
    };
    let order = journal.require_order()?;
    if order.root != root {
        return Err(format!(
            "capture stage: journal vault identity mismatch at {}",
            journal.dir.join("state.json").display()
        ));
    }
    let mut stages = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "capture stage: read entry under {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|error| format!("capture stage: inspect {}: {error}", path.display()))?
            .is_file()
        {
            return Err(format!(
                "capture stage: unexpected entry at {}",
                path.display()
            ));
        }
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| format!("capture stage: invalid name at {}", path.display()))?;
        if recover_atomic_temps && is_atomic_stage_temp(name) {
            fs::remove_file(&path).map_err(|error| {
                format!(
                    "capture recovery: remove atomic stage temp {}: {error}",
                    path.display()
                )
            })?;
            continue;
        }
        let stem = name
            .strip_suffix(".json")
            .ok_or_else(|| format!("capture stage: invalid name at {}", path.display()))?;
        let (sequence, file_request_id) = stem
            .split_once('-')
            .ok_or_else(|| format!("capture stage: invalid name at {}", path.display()))?;
        let sequence = sequence
            .parse::<u64>()
            .map_err(|_| format!("capture stage: invalid sequence at {}", path.display()))?;
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("capture stage: read {}: {error}", path.display()))?;
        let document: Value = serde_json::from_str(&text)
            .map_err(|error| format!("capture stage: parse {}: {error}", path.display()))?;
        if document.get("root").and_then(Value::as_str) != Some(root)
            || document.get("sequence").and_then(Value::as_u64) != Some(sequence)
        {
            return Err(format!(
                "capture stage: root or sequence mismatch at {}",
                path.display()
            ));
        }
        let request_id = document
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "capture stage: missing request identity at {}",
                    path.display()
                )
            })?;
        if uuid::Uuid::parse_str(request_id).is_err() {
            return Err(format!(
                "capture stage: invalid request identity at {}",
                path.display()
            ));
        }
        let envelope = document
            .get("envelope")
            .cloned()
            .ok_or_else(|| format!("capture stage: missing envelope at {}", path.display()))?;
        if request_id != file_request_id
            || envelope.get("request_id").and_then(Value::as_str) != Some(request_id)
            || envelope.get("session_id").and_then(Value::as_str) != Some(sid)
        {
            return Err(format!(
                "capture stage: envelope identity mismatch at {}",
                path.display()
            ));
        }
        if sequence >= order.next_to_drain {
            let request = order.pending.get(&sequence).ok_or_else(|| {
                format!(
                    "capture stage: sequence outside journal at {}",
                    path.display()
                )
            })?;
            let request = request.as_object().expect("validated journal request");
            if request
                .iter()
                .any(|(key, value)| envelope.get(key) != Some(value))
            {
                return Err(format!(
                    "capture stage: envelope conflicts with journal at {}",
                    path.display()
                ));
            }
        }
        if stages
            .insert(
                sequence,
                Stage {
                    path,
                    sequence,
                    envelope,
                },
            )
            .is_some()
        {
            return Err(format!(
                "capture stage: duplicate sequence {sequence} under {}",
                directory.display()
            ));
        }
    }
    Ok(stages)
}

const IO_CHUNK: usize = 64 * 1024;

struct RawGeneration {
    _directory: SessionDirectory,
    file: fs::File,
    path: PathBuf,
}

impl RawGeneration {
    fn open(directory: &Path, create: bool) -> Result<Option<Self>, String> {
        let directory_handle = SessionDirectory::open(directory)
            .map_err(|error| format!("capture commit: {error}"))?;
        let Some(file) = directory_handle
            .open_append("turns.jsonl", create)
            .map_err(|error| format!("capture commit: {error}"))?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            _directory: directory_handle,
            file,
            path: directory.join("turns.jsonl"),
        }))
    }

    fn read_exact_at(&self, mut bytes: &mut [u8], mut offset: u64) -> Result<(), String> {
        while !bytes.is_empty() {
            let read = self.file.read_at(bytes, offset).map_err(|error| {
                format!("capture commit: read {}: {error}", self.path.display())
            })?;
            if read == 0 {
                return Err(format!(
                    "capture commit: unexpected end of {}",
                    self.path.display()
                ));
            }
            offset += read as u64;
            bytes = &mut bytes[read..];
        }
        Ok(())
    }

    fn find_forward(
        &self,
        range: Range<u64>,
        predicate: impl Fn(u8) -> bool,
    ) -> Result<Option<u64>, String> {
        let mut offset = range.start;
        let mut chunk = vec![0; IO_CHUNK];
        while offset < range.end {
            let len = (range.end - offset).min(IO_CHUNK as u64) as usize;
            self.read_exact_at(&mut chunk[..len], offset)?;
            if let Some(position) = chunk[..len].iter().position(|byte| predicate(*byte)) {
                return Ok(Some(offset + position as u64));
            }
            offset += len as u64;
        }
        Ok(None)
    }

    fn find_backward(
        &self,
        range: Range<u64>,
        predicate: impl Fn(u8) -> bool,
    ) -> Result<Option<u64>, String> {
        let mut end = range.end;
        let mut chunk = vec![0; IO_CHUNK];
        while end > range.start {
            let start = end.saturating_sub(IO_CHUNK as u64).max(range.start);
            let len = (end - start) as usize;
            self.read_exact_at(&mut chunk[..len], start)?;
            if let Some(position) = chunk[..len].iter().rposition(|byte| predicate(*byte)) {
                return Ok(Some(start + position as u64));
            }
            end = start;
        }
        Ok(None)
    }

    fn range_matches_prefix(&self, range: &Range<u64>, expected: &[u8]) -> Result<bool, String> {
        let length = range.end - range.start;
        if length > expected.len() as u64 {
            return Ok(false);
        }
        let mut offset = 0usize;
        let mut chunk = vec![0; IO_CHUNK];
        while offset < length as usize {
            let len = (length as usize - offset).min(IO_CHUNK);
            self.read_exact_at(&mut chunk[..len], range.start + offset as u64)?;
            if chunk[..len] != expected[offset..offset + len] {
                return Ok(false);
            }
            offset += len;
        }
        Ok(true)
    }

    fn append_record(&mut self, serialized: &[u8]) -> Result<(), String> {
        self.file
            .write_all(serialized)
            .and_then(|_| self.file.write_all(b"\n"))
            .map_err(|error| format!("capture commit: append {}: {error}", self.path.display()))
    }

    fn truncate(&self, offset: u64) -> Result<(), String> {
        self.file
            .set_len(offset)
            .map_err(|error| format!("capture commit: truncate {}: {error}", self.path.display()))
    }
}

struct FileRange<'a> {
    raw: &'a RawGeneration,
    offset: u64,
    end: u64,
}

impl Read for FileRange<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let len = (self.end - self.offset).min(bytes.len() as u64) as usize;
        if len == 0 {
            return Ok(0);
        }
        let read = self.raw.file.read_at(&mut bytes[..len], self.offset)?;
        self.offset += read as u64;
        Ok(read)
    }
}

#[derive(Deserialize)]
struct EnvelopeIdentity {
    request_id: String,
}

enum CaptureTail {
    Blank,
    ValidTerminated {
        range: Range<u64>,
        request_id: String,
    },
    MalformedTerminated,
    Unterminated {
        range: Range<u64>,
    },
}

fn capture_tail(raw: &RawGeneration) -> Result<CaptureTail, String> {
    let length = raw
        .file
        .metadata()
        .map_err(|error| format!("capture commit: stat {}: {error}", raw.path.display()))?
        .len();
    let Some(last_content) = raw.find_backward(0..length, |byte| !byte.is_ascii_whitespace())?
    else {
        return Ok(CaptureTail::Blank);
    };
    let start = raw
        .find_backward(0..last_content, |byte| byte == b'\n')?
        .map_or(0, |newline| newline + 1);
    let Some(record_end) = raw.find_forward(last_content + 1..length, |byte| byte == b'\n')? else {
        return Ok(CaptureTail::Unterminated {
            range: start..length,
        });
    };

    let mut invalid_identity = false;
    let mut final_value = None;
    let decoded = vaultr::recon::decode_concatenated(
        FileRange {
            raw,
            offset: start,
            end: record_end,
        },
        |identity: EnvelopeIdentity, range| {
            if uuid::Uuid::parse_str(&identity.request_id).is_err() {
                invalid_identity = true;
            }
            final_value = Some((range, identity.request_id));
        },
    );
    let Some((range, request_id)) = final_value else {
        return Ok(CaptureTail::MalformedTerminated);
    };
    if decoded.is_err() || invalid_identity {
        return Ok(CaptureTail::MalformedTerminated);
    }
    let range_start = start + range.start as u64;
    let range_end = start + range.end as u64;
    let Some(value_start) =
        raw.find_forward(range_start..range_end, |byte| !byte.is_ascii_whitespace())?
    else {
        return Ok(CaptureTail::MalformedTerminated);
    };
    let value_end = raw
        .find_backward(value_start..range_end, |byte| !byte.is_ascii_whitespace())?
        .expect("value range has non-whitespace")
        + 1;
    Ok(CaptureTail::ValidTerminated {
        range: value_start..value_end,
        request_id,
    })
}

fn reconcile_append(raw: &mut RawGeneration, envelope: &Value) -> Result<(), String> {
    let serialized = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    let request_id = envelope.get("request_id").and_then(Value::as_str);
    match capture_tail(raw)? {
        CaptureTail::Blank => raw.append_record(&serialized),
        CaptureTail::ValidTerminated {
            range,
            request_id: tail_request_id,
        } if Some(tail_request_id.as_str()) == request_id => {
            if range.end - range.start == serialized.len() as u64
                && raw.range_matches_prefix(&range, &serialized)?
            {
                Ok(())
            } else {
                Err("capture commit: committed envelope conflicts with stage".into())
            }
        }
        CaptureTail::ValidTerminated { .. } => raw.append_record(&serialized),
        CaptureTail::MalformedTerminated => {
            Err("capture commit: malformed terminated capture tail".into())
        }
        CaptureTail::Unterminated { range } if raw.range_matches_prefix(&range, &serialized)? => {
            raw.truncate(range.start)?;
            raw.append_record(&serialized)
        }
        CaptureTail::Unterminated { .. } => {
            Err("capture commit: persisted tail conflicts with stage".into())
        }
    }
}

fn committed_exactly(raw: &RawGeneration, envelope: &Value) -> Result<bool, String> {
    let serialized = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    let CaptureTail::ValidTerminated { range, .. } = capture_tail(raw)? else {
        return Ok(false);
    };
    Ok(range.end - range.start == serialized.len() as u64
        && raw.range_matches_prefix(&range, &serialized)?)
}

fn commit_stage(journal: &mut Journal, stage: &Stage) -> Result<(), String> {
    let next = journal.require_order()?.next_to_drain;
    if stage.sequence < next {
        let committed = match RawGeneration::open(&journal.dir, false)? {
            Some(raw) => committed_exactly(&raw, &stage.envelope)?,
            None => false,
        };
        if stage.sequence + 1 != next || !committed {
            return Err(format!(
                "capture commit: retired stage conflicts at {}",
                stage.path.display()
            ));
        }
        return fs::remove_file(&stage.path).map_err(|error| {
            format!(
                "capture commit: remove retired stage {}: {error}",
                stage.path.display()
            )
        });
    }
    if stage.sequence != next {
        return Err(format!(
            "capture commit: stage sequence gap at {}",
            stage.path.display()
        ));
    }
    let mut raw = RawGeneration::open(&journal.dir, true)?
        .expect("create=true returns a raw generation handle");
    reconcile_append(&mut raw, &stage.envelope)?;
    {
        let order = journal.require_order_mut()?;
        order.next_to_drain += 1;
        order.pending.remove(&stage.sequence);
    }
    journal.persist()?;
    fs::remove_file(&stage.path).map_err(|error| {
        format!(
            "capture commit: remove stage {}: {error}",
            stage.path.display()
        )
    })
}

fn drain(root: &str, sid: &str, journal: &mut Journal) -> Result<(), String> {
    let mut stages = read_stages(root, sid, journal, false)?;
    loop {
        let sequence = journal.require_order()?.next_to_drain;
        if sequence >= journal.require_order()?.next_sequence {
            break;
        }
        let Some(stage) = stages.remove(&sequence) else {
            break;
        };
        commit_stage(journal, &stage)?;
    }
    Ok(())
}

pub(super) async fn reserve(
    dir: &Path,
    root: &str,
    sid: &str,
    build: impl FnOnce(&Value) -> (Value, Map<String, Value>),
) -> Result<(u64, Value), String> {
    let lock = session_lock(root, sid);
    let _guard = lock.lock().await;
    let mut journal = Journal::load(dir, sid)?;
    let prior = journal.prior_body();
    let (request, state) = build(&prior);
    let sequence = journal.reserve(root, request.clone())?;
    journal.replace_state(state);
    journal.persist()?;
    Ok((sequence, request))
}

pub(super) async fn commit_completed(
    dir: &Path,
    root: &str,
    sid: &str,
    sequence: u64,
    envelope: Value,
    on_staged: impl FnOnce(),
) -> Result<(), String> {
    let lock = session_lock(root, sid);
    let _guard = lock.lock().await;
    let mut journal = Journal::load(dir, sid)?;
    Stage::publish(root, sid, sequence, envelope)?;
    on_staged();
    drain(root, sid, &mut journal)
}

pub(super) enum SealReadiness {
    Detached(vaultr::vault::DetachedGeneration),
    Raw,
}

/// Validate the complete journal/stage boundary while the caller holds the
/// capture session mutex. Generation mutation is intentionally owned by the
/// sibling generation module and cannot enter below this gate.
pub(super) fn sealing_readiness(
    root: &str,
    sid: &str,
    dir: &Path,
) -> Result<Option<SealReadiness>, String> {
    let generations =
        vaultr::vault::CaptureGenerations::load(dir).map_err(|error| error.to_string())?;
    let journal = Journal::load(dir, sid)?;
    if let Some(detached) = generations.detached {
        return Ok(Some(SealReadiness::Detached(detached)));
    }

    let stage_dir = staging_dir(root, sid);
    let staged = if journal.order.is_some() {
        !read_stages(root, sid, &journal, false)?.is_empty()
    } else {
        match fs::read_dir(&stage_dir) {
            Ok(mut entries) => entries
                .next()
                .transpose()
                .map_err(|error| {
                    format!(
                        "capture stage: inspect entries under {}: {error}",
                        stage_dir.display()
                    )
                })?
                .is_some(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(format!(
                    "capture stage: inspect {}: {error}",
                    stage_dir.display()
                ));
            }
        }
    };
    if staged && (!journal.existed || journal.order.is_none()) {
        return Err(format!(
            "capture stage: no ordered journal for {}",
            stage_dir.display()
        ));
    }
    if let Some(order) = &journal.order {
        if order.root != root {
            return Err(format!(
                "capture journal: vault identity mismatch at {}",
                journal.dir.join("state.json").display()
            ));
        }
        if order.next_to_drain < order.next_sequence || staged {
            return Ok(None);
        }
    }

    if generations.raw.is_none() {
        return Ok(None);
    }
    Ok(Some(SealReadiness::Raw))
}

struct RecoverySession {
    sid: String,
    root: String,
    journal: Journal,
    stages: BTreeMap<u64, Stage>,
}

impl RecoverySession {
    fn apply(mut self) -> Result<(), String> {
        let next_to_drain = self.journal.require_order()?.next_to_drain;
        let retired: Vec<u64> = self
            .stages
            .keys()
            .copied()
            .take_while(|sequence| *sequence < next_to_drain)
            .collect();
        for sequence in retired {
            let stage = self.stages.remove(&sequence).unwrap();
            commit_stage(&mut self.journal, &stage)?;
        }
        loop {
            let order = self.journal.require_order()?;
            if order.next_to_drain >= order.next_sequence {
                break;
            }
            let sequence = order.next_to_drain;
            let stage = match self.stages.remove(&sequence) {
                Some(stage) => stage,
                None => {
                    let mut envelope = order.pending.get(&sequence).cloned().ok_or_else(|| {
                        format!(
                            "capture recovery: missing request sequence {sequence} at {}",
                            self.journal.dir.join("state.json").display()
                        )
                    })?;
                    envelope
                        .as_object_mut()
                        .expect("validated journal request")
                        .insert("response".into(), json!({ "complete": false }));
                    Stage::publish(&self.root, &self.sid, sequence, envelope)?
                }
            };
            commit_stage(&mut self.journal, &stage)?;
        }
        if let Some((_, stage)) = self.stages.into_iter().next() {
            return Err(format!(
                "capture recovery: unreconciled stage at {}",
                stage.path.display()
            ));
        }
        let directory = staging_dir(&self.root, &self.sid);
        if directory.exists() {
            fs::remove_dir(&directory).map_err(|error| {
                format!(
                    "capture recovery: remove stage directory {}: {error}",
                    directory.display()
                )
            })?;
        }
        if let Some(hash_dir) = directory.parent() {
            match fs::remove_dir(hash_dir) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(format!(
                        "capture recovery: remove staging root {}: {error}",
                        hash_dir.display()
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn recover_all(vault: &Path) -> Result<(), String> {
    let root = canonical_root(vault);
    let mut directories = BTreeMap::new();
    let mut journals = BTreeMap::new();
    let mut sessions = BTreeSet::new();
    for (sid, directory) in
        vaultr::vault::walk_session_dirs(vault).map_err(|error| error.to_string())?
    {
        if directories.insert(sid.clone(), directory.clone()).is_some() {
            return Err(format!(
                "capture recovery: duplicate session {sid} under {}",
                vault.display()
            ));
        }
        if !directory.join("state.json").exists() {
            continue;
        }
        let journal = Journal::load(&directory, &sid)?;
        if journal
            .order
            .as_ref()
            .is_some_and(|order| order.next_to_drain < order.next_sequence)
        {
            sessions.insert(sid.clone());
        }
        journals.insert(sid, journal);
    }

    let hash_dir = staging_base().join(vaultr::vault::sha256_hex(root.as_bytes()));
    if hash_dir.exists() {
        for entry in fs::read_dir(&hash_dir).map_err(|error| {
            format!(
                "capture recovery: read current-root staging {}: {error}",
                hash_dir.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                format!(
                    "capture recovery: read current-root staging {}: {error}",
                    hash_dir.display()
                )
            })?;
            let path = entry.path();
            if !entry
                .file_type()
                .map_err(|error| {
                    format!(
                        "capture recovery: inspect stage {}: {error}",
                        path.display()
                    )
                })?
                .is_dir()
            {
                return Err(format!(
                    "capture recovery: unexpected current-root stage entry at {}",
                    path.display()
                ));
            }
            let sid = entry
                .file_name()
                .to_str()
                .ok_or_else(|| {
                    format!(
                        "capture recovery: invalid staged session name at {}",
                        path.display()
                    )
                })?
                .to_string();
            sessions.insert(sid);
        }
    }

    let mut inventory = Vec::new();
    for sid in sessions {
        let directory = directories.get(&sid).ok_or_else(|| {
            format!(
                "capture recovery: staged session {sid} has no discovered directory under {}",
                vault.display()
            )
        })?;
        let journal = journals.remove(&sid).ok_or_else(|| {
            format!(
                "capture recovery: staged session {sid} has no journal at {}",
                directory.display()
            )
        })?;
        let order = journal.require_order()?;
        if order.root != root {
            return Err(format!(
                "capture recovery: journal vault identity mismatch at {}",
                directory.join("state.json").display()
            ));
        }
        let stages = read_stages(&root, &sid, &journal, true)?;
        inventory.push(RecoverySession {
            sid,
            root: root.clone(),
            journal,
            stages,
        });
    }
    for session in inventory {
        session.apply()?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn has_open_capture(vault: &Path, sid: &str) -> bool {
    let Ok(directory) = super::session_dir(vault, sid) else {
        return false;
    };
    let root = canonical_root(vault);
    let Ok(journal) = Journal::load(&directory, sid) else {
        return true;
    };
    journal
        .order
        .as_ref()
        .is_some_and(|order| order.next_to_drain < order.next_sequence)
        || fs::read_dir(staging_dir(&root, sid))
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests;
