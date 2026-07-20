use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

pub(super) fn session_lock(root: &str, sid: &str) -> Arc<tokio::sync::Mutex<()>> {
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

fn read_stages(root: &str, sid: &str, journal: &Journal) -> Result<BTreeMap<u64, Stage>, String> {
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

fn append_record(path: &Path, serialized: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("capture commit: open {}: {error}", path.display()))?;
    file.write_all(serialized)
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| format!("capture commit: append {}: {error}", path.display()))
}

enum CaptureTail {
    Blank,
    ValidTerminated {
        bytes: Vec<u8>,
        request_id: Option<String>,
    },
    MalformedTerminated,
    Unterminated {
        bytes: Vec<u8>,
        offset: u64,
    },
}

fn capture_tail(path: &Path) -> Result<CaptureTail, String> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CaptureTail::Blank);
        }
        Err(error) => {
            return Err(format!("capture commit: open {}: {error}", path.display()));
        }
    };
    let length = file
        .metadata()
        .map_err(|error| format!("capture commit: stat {}: {error}", path.display()))?
        .len();
    if length == 0 {
        return Ok(CaptureTail::Blank);
    }

    file.seek(SeekFrom::End(-1))
        .map_err(|error| format!("capture commit: seek {}: {error}", path.display()))?;
    let mut last_byte = [0];
    file.read_exact(&mut last_byte)
        .map_err(|error| format!("capture commit: read {}: {error}", path.display()))?;
    let terminated = last_byte[0] == b'\n';
    let end = length - u64::from(terminated);
    if end == 0 {
        return Ok(CaptureTail::Blank);
    }

    const CHUNK_SIZE: usize = 64 * 1024;
    let mut cursor = end;
    let mut start = 0;
    let mut chunk = vec![0; CHUNK_SIZE];
    while cursor > 0 {
        let chunk_start = cursor.saturating_sub(CHUNK_SIZE as u64);
        let chunk_len = (cursor - chunk_start) as usize;
        file.seek(SeekFrom::Start(chunk_start))
            .map_err(|error| format!("capture commit: seek {}: {error}", path.display()))?;
        file.read_exact(&mut chunk[..chunk_len])
            .map_err(|error| format!("capture commit: read {}: {error}", path.display()))?;
        if let Some(position) = chunk[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
            start = chunk_start + position as u64 + 1;
            break;
        }
        cursor = chunk_start;
    }

    let mut bytes = vec![0; (end - start) as usize];
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("capture commit: seek {}: {error}", path.display()))?;
    file.read_exact(&mut bytes)
        .map_err(|error| format!("capture commit: read {}: {error}", path.display()))?;

    if !terminated {
        return Ok(CaptureTail::Unterminated {
            bytes,
            offset: start,
        });
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(CaptureTail::Blank);
    }

    match serde_json::from_slice::<Value>(&bytes) {
        Ok(record) => Ok(CaptureTail::ValidTerminated {
            request_id: record
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            bytes,
        }),
        Err(_) => Ok(CaptureTail::MalformedTerminated),
    }
}

fn truncate(path: &Path, offset: u64) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(offset))
        .map_err(|error| format!("capture commit: truncate {}: {error}", path.display()))
}

fn reconcile_append(path: &Path, envelope: &Value) -> Result<(), String> {
    let serialized = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    let request_id = envelope.get("request_id").and_then(Value::as_str);
    match capture_tail(path)? {
        CaptureTail::Blank => append_record(path, &serialized),
        CaptureTail::ValidTerminated {
            bytes,
            request_id: tail_request_id,
        } if tail_request_id.as_deref() == request_id => {
            if bytes == serialized {
                Ok(())
            } else {
                Err("capture commit: committed envelope conflicts with stage".into())
            }
        }
        CaptureTail::ValidTerminated { .. } => append_record(path, &serialized),
        CaptureTail::MalformedTerminated => {
            Err("capture commit: malformed terminated capture tail".into())
        }
        CaptureTail::Unterminated { bytes, offset } if serialized.starts_with(&bytes) => {
            truncate(path, offset)?;
            append_record(path, &serialized)
        }
        CaptureTail::Unterminated { .. } => {
            Err("capture commit: persisted tail conflicts with stage".into())
        }
    }
}

fn committed_exactly(path: &Path, envelope: &Value) -> Result<bool, String> {
    let serialized = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    Ok(matches!(
        capture_tail(path)?,
        CaptureTail::ValidTerminated { bytes, .. } if bytes == serialized
    ))
}

fn commit_stage(journal: &mut Journal, stage: &Stage) -> Result<(), String> {
    let turns = journal.dir.join("turns.jsonl");
    let next = journal.require_order()?.next_to_drain;
    if stage.sequence < next {
        if stage.sequence + 1 != next || !committed_exactly(&turns, &stage.envelope)? {
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
    reconcile_append(&turns, &stage.envelope)?;
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
    let mut stages = read_stages(root, sid, journal)?;
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

pub(crate) async fn detach_generation(
    vault: &Path,
    sid: &str,
    dir: &Path,
) -> Result<Option<vaultr::vault::DetachedGeneration>, String> {
    let root = canonical_root(vault);
    let lock = session_lock(&root, sid);
    let _guard = lock.lock().await;
    let generations =
        vaultr::vault::CaptureGenerations::load(dir).map_err(|error| error.to_string())?;
    let journal = Journal::load(dir, sid)?;
    if let Some(detached) = generations.detached {
        return Ok(Some(detached));
    }

    let stage_dir = staging_dir(&root, sid);
    let staged = if journal.order.is_some() {
        !read_stages(&root, sid, &journal)?.is_empty()
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

    let Some(raw) = generations.raw else {
        return Ok(None);
    };
    if !crate::sweep::scrub(&raw).await {
        return Err(format!("scrub generation failed at {}", raw.display()));
    }
    let digest = vaultr::vault::sha256_file(&raw).map_err(|error| error.to_string())?;
    let base_len = generations
        .sealed
        .as_ref()
        .map(fs::metadata)
        .transpose()
        .map_err(|error| format!("stat sealed generation under {}: {error}", dir.display()))?
        .map_or(0, |metadata| metadata.len());
    let path = dir.join(format!("turns.jsonl.sealing-{base_len}-{digest}"));
    fs::rename(&raw, &path)
        .map_err(|error| format!("detach generation {}: {error}", raw.display()))?;
    Ok(Some(vaultr::vault::DetachedGeneration {
        path,
        base_len,
        digest,
    }))
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
        let stages = read_stages(&root, &sid, &journal)?;
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
