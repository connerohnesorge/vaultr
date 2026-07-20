//! Vault capture — envelope writer, state.json, meta merge. Delta encoding
//! lives in vaultr::recon next to its inverse.
//! Ports capture/sessionDir/updateMeta (wireproxy.ts:122-422).

use crate::adapter::{Adapter, Identity};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use vaultr::recon;
use vaultr::vault::{dated_session_dir, Meta};

pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub content_encoding: Option<String>,
    pub body_sha256: String,
    pub ids: Identity,
    pub started_at: SystemTime,
}

/// Delta computed and state.json written at request time, so the multi-MB body
/// Value is dropped before streaming starts. Only this small struct is held
/// for the (minutes-long) stream duration — the memory fix over the Bun version.
pub struct PendingCapture {
    pub dir: PathBuf,
    /// Canonical Session Capture root at reservation time — validates staged
    /// evidence against the vault identity during recovery.
    pub root: String,
    /// Private per-session preparation sequence. Envelopes persist in this order
    /// (delta bases advance during preparation), NOT completion order.
    pub sequence: u64,
    pub request_part: Value, // envelope minus response
    pub model: Option<String>,
    pub req: CapturedRequest,
}

/// Private per-session ordering journal, embedded in `state.json` under
/// `capture_order`. Legacy files (no such key) load as default and begin
/// sequencing lazily from 0 on the first new reservation, preserving the
/// existing `request_body` delta base. Never part of the public Envelope schema.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CaptureOrder {
    #[serde(default)]
    next_sequence: u64,
    #[serde(default)]
    next_to_drain: u64,
    /// Reserved-but-not-yet-drained request halves, keyed by sequence, so a
    /// restart can materialize abandoned preparations at their reserved slot.
    #[serde(default)]
    pending: BTreeMap<u64, Value>,
    /// Canonical Session Capture root recorded at first reservation.
    #[serde(default)]
    root: String,
}

pub struct CapturedResponse {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub sse: String,
    pub complete: bool,
}

static SESSION_DIRS: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

/// Session ids this process is actively capturing since startup — i.e. every
/// session that hit `session_dir` for `vault`, not everything on disk.
pub(crate) fn cached_session_ids(vault: &Path) -> Vec<String> {
    let prefix = format!("{}\0", vault.display());
    SESSION_DIRS
        .lock()
        .unwrap()
        .as_ref()
        .map(|dirs| {
            dirs.keys()
                .filter_map(|key| key.strip_prefix(&prefix).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Ordered-persistence machinery (#16): a private per-session async mutex
// serializes journal mutation, stage publication, and draining; completed
// Envelopes stage outside the Git-backed vault and drain in preparation order.
// ---------------------------------------------------------------------------

static SESSION_LOCKS: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    Mutex::new(None);

/// Per-(canonical-root, session) async mutex. Held for journal mutation, stage
/// publication, and draining — NEVER across upstream response streaming.
fn session_lock(root: &str, sid: &str) -> Arc<tokio::sync::Mutex<()>> {
    let key = format!("{root}\0{sid}");
    SESSION_LOCKS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Canonical Session Capture root — the vault identity that keys staging and
/// guards recovery against a moved vault. Falls back to the given path when the
/// vault dir does not yet exist (first-run / tests).
pub(crate) fn canonical_root(vault: &Path) -> String {
    fs::canonicalize(vault)
        .unwrap_or_else(|_| vault.to_path_buf())
        .display()
        .to_string()
}

fn staging_base() -> PathBuf {
    crate::jobs::state_dir().join("capture-staging")
}

fn staging_dir(root: &str, sid: &str) -> PathBuf {
    staging_base().join(sha256_hex(root.as_bytes())).join(sid)
}

/// Locate the staged completed Envelope for a sequence, if any (`<seq>-<rid>.json`).
fn find_stage(root: &str, sid: &str, seq: u64) -> Option<PathBuf> {
    let dir = staging_dir(root, sid);
    let prefix = format!("{seq}-");
    fs::read_dir(&dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        let name = name.to_str()?;
        (name.starts_with(&prefix) && name.ends_with(".json")).then(|| e.path())
    })
}

/// Write-then-rename: atomic replacement within a filesystem. Per-request
/// `fsync` is intentionally omitted (design non-goal) — process-crash safety
/// comes from atomic rename, not power-loss durability.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    fs::File::create(&tmp)?.write_all(bytes)?;
    fs::rename(&tmp, path)
}

fn read_state(dir: &Path) -> Map<String, Value> {
    fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn read_order(state: &Map<String, Value>) -> CaptureOrder {
    state
        .get("capture_order")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn read_state_strict(path: &Path) -> Result<Map<String, Value>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("capture recovery: read {}: {e}", path.display()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(state)) => Ok(state),
        Ok(_) => Err(format!(
            "capture recovery: journal {} is not a JSON object",
            path.display()
        )),
        Err(e) => Err(format!(
            "capture recovery: parse journal {}: {e}",
            path.display()
        )),
    }
}

fn read_order_strict(
    state: &Map<String, Value>,
    journal: &Path,
) -> Result<Option<CaptureOrder>, String> {
    let Some(value) = state.get("capture_order") else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|e| {
            format!(
                "capture recovery: invalid capture_order in {}: {e}",
                journal.display()
            )
        })
}

/// Persist a state map with the given ordering journal, atomically.
fn persist_state(
    dir: &Path,
    mut state: Map<String, Value>,
    order: &CaptureOrder,
) -> Result<(), String> {
    state.insert(
        "capture_order".into(),
        serde_json::to_value(order).map_err(|e| e.to_string())?,
    );
    let text = serde_json::to_string(&Value::Object(state)).map_err(|e| e.to_string())? + "\n";
    atomic_write(&dir.join("state.json"), text.as_bytes()).map_err(|e| e.to_string())
}

/// Append one Envelope as a JSONL line.
fn append_line(turns: &Path, env: &Value) -> Result<(), String> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(turns)
        .map_err(|e| e.to_string())?;
    writeln!(
        f,
        "{}",
        serde_json::to_string(env).map_err(|e| e.to_string())?
    )
    .map_err(|e| e.to_string())
}

/// Read the final line of `turns.jsonl` (bounded to `max_tail` trailing bytes,
/// always >= the candidate Envelope size so a full/committed line is visible):
/// returns (line, terminated_by_newline, byte_offset_of_line_start).
fn last_line(turns: &Path, max_tail: usize) -> Result<Option<(String, bool, u64)>, String> {
    let mut f = match fs::File::open(turns) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len == 0 {
        return Ok(None);
    }
    let window = (max_tail as u64).min(len);
    let from = len - window;
    f.seek(SeekFrom::Start(from)).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(window as usize);
    f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let terminated = buf.last() == Some(&b'\n');
    let end = buf.len() - terminated as usize;
    let body = &buf[..end];
    let nl = body.iter().rposition(|&b| b == b'\n');
    let (start_in_buf, line_bytes) = match nl {
        Some(pos) => (pos + 1, &body[pos + 1..]),
        None => (0, body),
    };
    let line = String::from_utf8_lossy(line_bytes).into_owned();
    Ok(Some((line, terminated, from + start_in_buf as u64)))
}

fn truncate_at(turns: &Path, offset: u64) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .open(turns)
        .and_then(|f| f.set_len(offset))
        .map_err(|e| e.to_string())
}

/// Idempotently append `env` at recovery time, reconciling the two crash windows
/// in the drain commit order (append → journal-retire → stage-delete):
/// - a complete final line equal to `env` (same request_id) is already committed;
/// - an unterminated final line that is an exact byte prefix of `env` is a
///   crashed append — truncate it and write the full record;
/// - a same-id-but-different complete line, or a nonmatching unterminated tail,
///   is a conflict: leave bytes unchanged and fail.
fn reconcile_append(turns: &Path, env: &Value) -> Result<(), String> {
    let serialized = serde_json::to_string(env).map_err(|e| e.to_string())?;
    let rid = env.get("request_id").and_then(Value::as_str);
    match last_line(turns, serialized.len() + 4096)? {
        None => append_line(turns, env),
        Some((last, terminated, tail_start)) => {
            if terminated {
                match serde_json::from_str::<Value>(&last) {
                    Ok(v) if v.get("request_id").and_then(Value::as_str) == rid => {
                        if &v == env {
                            Ok(()) // already committed — no duplicate
                        } else {
                            Err("recovery: committed envelope conflicts with stage".into())
                        }
                    }
                    _ => append_line(turns, env),
                }
            } else if serialized.as_bytes().starts_with(last.as_bytes()) {
                truncate_at(turns, tail_start)?;
                append_line(turns, env)
            } else {
                Err("recovery: persisted tail does not match staged envelope".into())
            }
        }
    }
}

/// True when a session has an open reservation or undrained completed stage —
/// its raw generation must not be sealed yet. Crate-private; no public state.
#[cfg(test)]
pub(crate) fn has_open_capture(vault: &Path, sid: &str) -> bool {
    let Ok(dir) = session_dir(vault, sid) else {
        return false;
    };
    has_open_capture_at(vault, sid, &dir)
}

#[cfg(test)]
fn has_open_capture_at(vault: &Path, sid: &str, dir: &Path) -> bool {
    let order = read_order(&read_state(&dir));
    if order.next_to_drain < order.next_sequence {
        return true;
    }
    let sdir = staging_dir(&canonical_root(vault), sid);
    fs::read_dir(&sdir)
        .map(|mut r| r.next().is_some())
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub(crate) struct DetachedGeneration {
    pub path: PathBuf,
    pub base_len: u64,
}

fn detached_generation(dir: &Path) -> Result<Option<DetachedGeneration>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("inspect detached generation {}: {e}", dir.display()))?;
    let mut found = None;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("inspect detached generation {}: {e}", dir.display()))?
            .path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("turns.jsonl.sealing-") else {
            continue;
        };
        let Some((base_len, digest)) = rest.split_once('-') else {
            return Err(format!(
                "invalid detached generation name at {}",
                path.display()
            ));
        };
        let base_len = base_len.parse::<u64>().map_err(|_| {
            format!(
                "invalid detached generation base length at {}",
                path.display()
            )
        })?;
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "invalid detached generation digest at {}",
                path.display()
            ));
        }
        if found.is_some() {
            return Err(format!(
                "multiple detached generations under {}",
                dir.display()
            ));
        }
        found = Some(DetachedGeneration { path, base_len });
    }
    Ok(found)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("hash generation {}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("hash generation {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Under the same per-session mutex as capture completion, recheck that the
/// current raw generation is closed and atomically detach it. A prior detached
/// generation is resumed before any newer raw generation is considered.
pub(crate) async fn detach_generation(
    vault: &Path,
    sid: &str,
    dir: &Path,
) -> Result<Option<DetachedGeneration>, String> {
    let root = canonical_root(vault);
    let lock = session_lock(&root, sid);
    let _guard = lock.lock().await;

    if let Some(detached) = detached_generation(dir)? {
        let expected = detached
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit_once('-'))
            .map(|(_, digest)| digest)
            .unwrap_or_default();
        if sha256_file(&detached.path)? != expected {
            return Err(format!(
                "detached generation digest mismatch at {}",
                detached.path.display()
            ));
        }
        return Ok(Some(detached));
    }

    let journal = dir.join("state.json");
    let order = if journal.exists() {
        let state = read_state_strict(&journal)?;
        read_order_strict(&state, &journal)?.unwrap_or_default()
    } else {
        CaptureOrder::default()
    };
    let staged = match fs::read_dir(staging_dir(&root, sid)) {
        Ok(mut entries) => entries
            .next()
            .transpose()
            .map_err(|e| {
                format!(
                    "inspect capture stages for {sid} under {}: {e}",
                    staging_dir(&root, sid).display()
                )
            })?
            .is_some(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(format!(
                "inspect capture stages for {sid} under {}: {e}",
                staging_dir(&root, sid).display()
            ))
        }
    };
    if order.next_to_drain < order.next_sequence || staged {
        return Ok(None);
    }

    let raw = dir.join("turns.jsonl");
    if !raw.is_file() {
        return Ok(None);
    }
    if !crate::sweep::scrub(&raw).await {
        return Err(format!("scrub generation failed at {}", raw.display()));
    }
    let digest = sha256_file(&raw)?;
    let base_len = fs::metadata(dir.join("turns.jsonl.zst"))
        .map(|meta| meta.len())
        .unwrap_or(0);
    let path = dir.join(format!("turns.jsonl.sealing-{base_len}-{digest}"));
    fs::rename(&raw, &path).map_err(|e| format!("detach generation {}: {e}", raw.display()))?;
    Ok(Some(DetachedGeneration { path, base_len }))
}

/// ISO-8601 UTC with milliseconds, matching JS Date.toISOString().
pub fn iso_now() -> String {
    iso_at(SystemTime::now())
}

pub fn iso_at(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let (secs, ms) = (d.as_secs() as i64, d.subsec_millis());
    let (y, mo, da, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// days-since-epoch -> civil date (Howard Hinnant's algorithm)
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// vault/YYYY/MM/DD/<session_id>, dated from meta original_start when parseable.
pub fn session_dir(vault: &Path, session_id: &str) -> std::io::Result<PathBuf> {
    let key = format!("{}\0{}", vault.display(), session_id);
    let mut guard = SESSION_DIRS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(dir) = map.get(&key) {
        return Ok(dir.clone());
    }
    let original_start = fs::read_to_string(vault.join(".meta").join(format!("{session_id}.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Meta>(&text).ok())
        .and_then(|meta| meta.original_start);
    let dir = dated_session_dir(vault, session_id, original_start.as_deref())
        .or_else(|| dated_session_dir(vault, session_id, Some(&iso_now())))
        .expect("iso_now returns RFC 3339");
    fs::create_dir_all(&dir)?;
    map.insert(key, dir.clone());
    Ok(dir)
}

fn allowed_headers(headers: &hyper::HeaderMap) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in headers {
        let name = k.as_str().to_ascii_lowercase();
        let allowed = matches!(
            name.as_str(),
            "content-type" | "request-id" | "x-request-id" | "retry-after"
        ) || name.starts_with("anthropic-ratelimit-")
            || name.starts_with("x-ratelimit-");
        if allowed {
            if let Ok(s) = v.to_str() {
                out.insert(name, Value::String(s.to_string()));
            }
        }
    }
    out
}

pub fn update_meta(
    vault: &Path,
    adapter: &Adapter,
    ids: &Identity,
    model: Option<&str>,
) -> std::io::Result<()> {
    let meta_dir = vault.join(".meta");
    fs::create_dir_all(&meta_dir)?;
    let path = meta_dir.join(format!(
        "{}.json",
        ids.session_id.as_deref().unwrap_or("unknown")
    ));
    let mut meta: Meta = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let now = iso_now();
    meta.schema_version = Some(1);
    meta.harness = Some(adapter.harness.to_string());
    meta.session_id.clone_from(&ids.session_id);
    if ids.thread_id.is_some() {
        meta.thread_id.clone_from(&ids.thread_id);
    }
    if let Some(model) = model {
        meta.model = Some(model.to_string());
    }
    meta.session_start_source
        .get_or_insert_with(|| "wire".to_string());
    meta.original_start.get_or_insert_with(|| now.clone());
    meta.last_observation = Some(now);
    fs::write(&path, to_string_pretty_1(&json!(meta)) + "\n")
}

/// Request-time half: reserve a preparation sequence, advance the delta base,
/// and record the request half in the journal — all atomically under the
/// per-session mutex, which is released before response streaming begins.
pub async fn prepare_capture(
    vault: &Path,
    adapter: &Adapter,
    req: CapturedRequest,
    body: Value,
) -> Result<PendingCapture, String> {
    let sid = req
        .ids
        .session_id
        .clone()
        .ok_or_else(|| format!("{} request has no session identity", adapter.harness))?;
    let dir = session_dir(vault, &sid).map_err(|e| e.to_string())?;
    let root = canonical_root(vault);

    let lock = session_lock(&root, &sid);
    let _guard = lock.lock().await;

    let state = read_state(&dir);
    let prior = state.get("request_body").cloned().unwrap_or(Value::Null);
    let body_delta = recon::encode_delta(&prior, &body, adapter.history_key, adapter.big_fields);

    let mut order = read_order(&state);
    if order.root.is_empty() {
        order.root = root.clone();
    }
    let sequence = order.next_sequence;
    order.next_sequence += 1;

    let request_part = json!({
        "schema_version": 1,
        "request_id": uuid::Uuid::new_v4().to_string(),
        "observed_at": iso_now(),
        "harness": adapter.harness,
        "session_id": sid,
        "thread_id": req.ids.thread_id,
        "turn_id": req.ids.turn_id,
        "request": {
            "method": req.method,
            "path": req.path,
            "content_encoding": req.content_encoding,
            "body_sha256": req.body_sha256,
            "body_delta": body_delta,
        },
    });
    order.pending.insert(sequence, request_part.clone());

    let model = body.get("model").and_then(Value::as_str).map(String::from);
    let new_state = json!({
        "schema_version": 1,
        "harness": adapter.harness,
        "session_id": sid,
        "thread_id": req.ids.thread_id,
        "request_body": body,
    });
    persist_state(
        &dir,
        new_state.as_object().cloned().unwrap_or_default(),
        &order,
    )?;

    Ok(PendingCapture {
        dir,
        root,
        sequence,
        request_part,
        model,
        req,
    })
}

/// Stream-end half: attach the response, durably STAGE the completed Envelope
/// (finish succeeds here even if an earlier live sequence blocks draining), run
/// the Session Index / Herdr side effects at durable stage acceptance, then
/// drain every eligible sequence into `turns.jsonl` in preparation order.
pub async fn finish_capture(
    vault: &Path,
    adapter: &Adapter,
    mut pending: PendingCapture,
    resp: &CapturedResponse,
) -> Result<(), String> {
    if let Some(obj) = pending.request_part.as_object_mut() {
        obj.insert(
            "response".into(),
            json!({
                "status": resp.status,
                "headers": allowed_headers(&resp.headers),
                "complete": resp.complete,
                "sse": resp.sse,
            }),
        );
    }
    let sid = pending
        .request_part
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let request_id = pending
        .request_part
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let lock = session_lock(&pending.root, &sid);
    let _guard = lock.lock().await;

    // Durable stage — the Envelope is safe from here even if draining can't yet
    // proceed. Stage records the canonical root so recovery can reject a mismatch.
    let stage_path =
        staging_dir(&pending.root, &sid).join(format!("{}-{request_id}.json", pending.sequence));
    let stage_doc = json!({
        "root": pending.root,
        "sequence": pending.sequence,
        "request_id": request_id,
        "envelope": pending.request_part,
    });
    atomic_write(
        &stage_path,
        serde_json::to_string(&stage_doc)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| format!("stage write: {e}"))?;

    // Side effects at durable stage acceptance — logged separately, never
    // reclassify an accepted stage as lost.
    crate::herdr::maybe_snapshot(vault);
    if let Err(e) = update_meta(vault, adapter, &pending.req.ids, pending.model.as_deref()) {
        eprintln!("[{}] meta update failed: {e}", adapter.harness);
    }

    drain(&pending.dir, &pending.root, &sid)
}

/// Drain every eligible sequence starting at `next_to_drain`, stopping at the
/// first gap (an earlier still-live response). Commit order per sequence:
/// append the Envelope, advance the journal + retire the pending half, delete
/// the stage — so any crash window is recoverable at startup.
fn drain(dir: &Path, root: &str, sid: &str) -> Result<(), String> {
    loop {
        let state = read_state(dir);
        let mut order = read_order(&state);
        let seq = order.next_to_drain;
        if seq >= order.next_sequence {
            break;
        }
        let Some(stage_path) = find_stage(root, sid, seq) else {
            break; // earlier live gap — later stages wait
        };
        let doc: Value =
            serde_json::from_str(&fs::read_to_string(&stage_path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let env = doc
            .get("envelope")
            .cloned()
            .ok_or_else(|| format!("stage {sid} seq {seq}: missing envelope"))?;
        append_line(&dir.join("turns.jsonl"), &env)?;
        order.next_to_drain = seq + 1;
        order.pending.remove(&seq);
        persist_state(dir, state, &order)?;
        let _ = fs::remove_file(&stage_path);
    }
    Ok(())
}

struct StageEvidence {
    path: PathBuf,
    envelope: Value,
}

fn validate_order(
    state: &Map<String, Value>,
    order: &CaptureOrder,
    journal: &Path,
    root: &str,
    sid: &str,
) -> Result<(), String> {
    if state.get("session_id").and_then(Value::as_str) != Some(sid) {
        return Err(format!(
            "capture recovery: journal identity mismatch at {}",
            journal.display()
        ));
    }
    if order.root != root {
        return Err(format!(
            "capture recovery: journal vault-identity mismatch at {}",
            journal.display()
        ));
    }
    if order.next_to_drain > order.next_sequence {
        return Err(format!(
            "capture recovery: invalid sequence bounds at {}",
            journal.display()
        ));
    }
    if order
        .pending
        .keys()
        .any(|seq| *seq < order.next_to_drain || *seq >= order.next_sequence)
    {
        return Err(format!(
            "capture recovery: pending sequence outside journal bounds at {}",
            journal.display()
        ));
    }
    for seq in order.next_to_drain..order.next_sequence {
        let Some(request) = order.pending.get(&seq) else {
            return Err(format!(
                "capture recovery: journal {} missing request for sequence {seq}",
                journal.display()
            ));
        };
        if request.get("session_id").and_then(Value::as_str) != Some(sid)
            || request.get("request_id").and_then(Value::as_str).is_none()
        {
            return Err(format!(
                "capture recovery: invalid request identity in {} sequence {seq}",
                journal.display()
            ));
        }
    }
    Ok(())
}

fn read_stages(
    sdir: &Path,
    root: &str,
    sid: &str,
    order: &CaptureOrder,
) -> Result<BTreeMap<u64, StageEvidence>, String> {
    if !sdir.exists() {
        return Ok(BTreeMap::new());
    }
    let entries = fs::read_dir(sdir).map_err(|e| {
        format!(
            "capture recovery: read stage directory {}: {e}",
            sdir.display()
        )
    })?;
    let mut stages = BTreeMap::new();
    for entry in entries {
        let path = entry
            .map_err(|e| format!("capture recovery: read stage entry {}: {e}", sdir.display()))?
            .path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("capture recovery: invalid stage name at {}", path.display()))?;
        let stem = name.strip_suffix(".json").ok_or_else(|| {
            format!(
                "capture recovery: unexpected stage entry at {}",
                path.display()
            )
        })?;
        let (sequence, file_request_id) = stem
            .split_once('-')
            .ok_or_else(|| format!("capture recovery: invalid stage name at {}", path.display()))?;
        let sequence = sequence.parse::<u64>().map_err(|_| {
            format!(
                "capture recovery: invalid stage sequence at {}",
                path.display()
            )
        })?;
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("capture recovery: read stage {}: {e}", path.display()))?;
        let doc: Value = serde_json::from_str(&text)
            .map_err(|e| format!("capture recovery: parse stage {}: {e}", path.display()))?;
        if doc.get("root").and_then(Value::as_str) != Some(root)
            || doc.get("sequence").and_then(Value::as_u64) != Some(sequence)
        {
            return Err(format!(
                "capture recovery: stage root or sequence mismatch at {}",
                path.display()
            ));
        }
        let request_id = doc
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "capture recovery: stage missing request identity at {}",
                    path.display()
                )
            })?;
        let envelope = doc.get("envelope").cloned().ok_or_else(|| {
            format!(
                "capture recovery: stage missing envelope at {}",
                path.display()
            )
        })?;
        if request_id != file_request_id
            || envelope.get("request_id").and_then(Value::as_str) != Some(request_id)
            || envelope.get("session_id").and_then(Value::as_str) != Some(sid)
        {
            return Err(format!(
                "capture recovery: stage envelope identity mismatch at {}",
                path.display()
            ));
        }
        if sequence >= order.next_to_drain {
            let request = order.pending.get(&sequence).ok_or_else(|| {
                format!(
                    "capture recovery: stage sequence outside journal at {}",
                    path.display()
                )
            })?;
            let request_obj = request.as_object().ok_or_else(|| {
                format!(
                    "capture recovery: invalid pending request for stage {}",
                    path.display()
                )
            })?;
            if request_obj
                .iter()
                .any(|(key, value)| envelope.get(key) != Some(value))
            {
                return Err(format!(
                    "capture recovery: stage does not match journal request at {}",
                    path.display()
                ));
            }
        }
        if stages
            .insert(sequence, StageEvidence { path, envelope })
            .is_some()
        {
            return Err(format!(
                "capture recovery: duplicate stage sequence {sequence} under {}",
                sdir.display()
            ));
        }
    }
    Ok(stages)
}

/// Recover every ordering journal and completed stage before Plant accepts proxy
/// traffic or permits Sealing. Inventory is bounded to the canonical current
/// root and retains each discovered evidence path through reconciliation.
pub fn recover_all(vault: &Path) -> Result<(), String> {
    let root = canonical_root(vault);
    let mut dirs = BTreeMap::new();
    let mut sessions = BTreeSet::new();
    for (sid, dir) in vaultr::vault::walk_session_dirs(vault).map_err(|e| e.to_string())? {
        if dirs.insert(sid.clone(), dir.clone()).is_some() {
            return Err(format!(
                "capture recovery: duplicate session {sid} under {}",
                vault.display()
            ));
        }
        let journal = dir.join("state.json");
        if !journal.exists() {
            continue;
        }
        let state = read_state_strict(&journal)?;
        if let Some(order) = read_order_strict(&state, &journal)? {
            validate_order(&state, &order, &journal, &root, &sid)?;
            if order.next_to_drain < order.next_sequence {
                sessions.insert(sid);
            }
        }
    }

    let hash_dir = staging_base().join(sha256_hex(root.as_bytes()));
    if hash_dir.exists() {
        let entries = fs::read_dir(&hash_dir).map_err(|e| {
            format!(
                "capture recovery: read current-root staging {}: {e}",
                hash_dir.display()
            )
        })?;
        for entry in entries {
            let path = entry
                .map_err(|e| {
                    format!(
                        "capture recovery: read current-root staging {}: {e}",
                        hash_dir.display()
                    )
                })?
                .path();
            if !path.is_dir() {
                return Err(format!(
                    "capture recovery: unexpected current-root stage entry at {}",
                    path.display()
                ));
            }
            let sid = path
                .file_name()
                .and_then(|name| name.to_str())
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

    // Validate every current-root journal and stage before reconciling any of
    // them, so malformed inventory cannot hide behind an earlier mutation.
    for sid in &sessions {
        let dir = dirs.get(sid).ok_or_else(|| {
            format!(
                "capture recovery: staged session {sid} has no discovered session directory under {}",
                vault.display()
            )
        })?;
        let journal = dir.join("state.json");
        let state = read_state_strict(&journal)?;
        let order = read_order_strict(&state, &journal)?.ok_or_else(|| {
            format!(
                "capture recovery: staged session {sid} has no capture_order in {}",
                journal.display()
            )
        })?;
        validate_order(&state, &order, &journal, &root, sid)?;
        read_stages(&staging_dir(&root, sid), &root, sid, &order)?;
    }

    for sid in sessions {
        let dir = dirs.get(&sid).expect("validated recovery session path");
        recover_session(dir, &root, &sid)?;
    }
    Ok(())
}

fn recover_session(dir: &Path, root: &str, sid: &str) -> Result<(), String> {
    let journal = dir.join("state.json");
    let mut state = read_state_strict(&journal)?;
    let mut order = read_order_strict(&state, &journal)?.ok_or_else(|| {
        format!(
            "capture recovery: staged session {sid} has no capture_order in {}",
            journal.display()
        )
    })?;
    validate_order(&state, &order, &journal, root, sid)?;

    let sdir = staging_dir(root, sid);
    let mut stages = read_stages(&sdir, root, sid, &order)?;
    let turns = dir.join("turns.jsonl");

    let retired: Vec<u64> = stages
        .keys()
        .copied()
        .take_while(|seq| *seq < order.next_to_drain)
        .collect();
    for seq in retired {
        let stage = stages.remove(&seq).unwrap();
        let serialized = serde_json::to_string(&stage.envelope).map_err(|e| e.to_string())?;
        let matches = last_line(&turns, serialized.len() + 4096)?
            .and_then(|(line, terminated, _)| {
                terminated
                    .then(|| serde_json::from_str::<Value>(&line).ok())
                    .flatten()
            })
            .is_some_and(|envelope| envelope == stage.envelope);
        if !matches || seq + 1 != order.next_to_drain {
            return Err(format!(
                "capture recovery: retired stage does not match committed evidence at {}",
                stage.path.display()
            ));
        }
        fs::remove_file(&stage.path).map_err(|e| {
            format!(
                "capture recovery: remove retired stage {}: {e}",
                stage.path.display()
            )
        })?;
    }

    let mut seq = order.next_to_drain;
    while seq < order.next_sequence {
        let stage = stages.remove(&seq);
        let env = match &stage {
            Some(stage) => stage.envelope.clone(),
            None => {
                let mut env = order.pending.get(&seq).cloned().ok_or_else(|| {
                    format!(
                        "capture recovery: journal {} missing request for sequence {seq}",
                        journal.display()
                    )
                })?;
                env.as_object_mut()
                    .expect("validated pending request object")
                    .insert("response".into(), json!({ "complete": false }));
                env
            }
        };
        reconcile_append(&turns, &env)?;
        order.next_to_drain = seq + 1;
        order.pending.remove(&seq);
        persist_state(dir, state.clone(), &order)?;
        state.insert(
            "capture_order".into(),
            serde_json::to_value(&order).map_err(|e| e.to_string())?,
        );
        if let Some(stage) = stage {
            fs::remove_file(&stage.path).map_err(|e| {
                format!(
                    "capture recovery: remove committed stage {}: {e}",
                    stage.path.display()
                )
            })?;
        }
        seq += 1;
    }
    if let Some((_, stage)) = stages.into_iter().next() {
        return Err(format!(
            "capture recovery: unreconciled stage at {}",
            stage.path.display()
        ));
    }
    if sdir.exists() {
        fs::remove_dir(&sdir).map_err(|e| {
            format!(
                "capture recovery: remove stage directory {}: {e}",
                sdir.display()
            )
        })?;
    }
    if let Some(hash_dir) = sdir.parent() {
        match fs::remove_dir(hash_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => {
                return Err(format!(
                    "capture recovery: remove staging root {}: {e}",
                    hash_dir.display()
                ))
            }
        }
    }
    Ok(())
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Return dirty freed pages to the OS — macOS malloc otherwise ratchets RSS
/// to the high-water mark of concurrent JSON DOM peaks.
pub fn release_memory() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// JSON.stringify(x, null, 1) equivalent: pretty print with 1-space indent.
pub fn to_string_pretty_1(value: &Value) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(value, &mut ser).expect("json serialize");
    String::from_utf8(buf).expect("utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("plant-{label}-{}", uuid::Uuid::new_v4()))
    }

    fn claude_adapter() -> Adapter {
        crate::adapter::adapters().remove(0)
    }

    fn captured(session_id: Option<&str>) -> CapturedRequest {
        CapturedRequest {
            method: "POST".into(),
            path: "/v1/messages".into(),
            content_encoding: None,
            body_sha256: "deadbeef".into(),
            ids: Identity {
                session_id: session_id.map(String::from),
                ..Default::default()
            },
            started_at: SystemTime::now(),
        }
    }

    fn delta(pending: &PendingCapture) -> &Value {
        &pending.request_part["request"]["body_delta"]
    }

    #[tokio::test]
    async fn prepare_capture_delta_lifecycle() {
        let vault = temp_vault("prep");
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();

        // First turn: no state.json — everything is new.
        let body1 = json!({
            "model": "m",
            "system": "sys",
            "tools": [{ "name": "t" }],
            "messages": [{ "role": "user", "content": "a" }],
        });
        let p1 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body1.clone())
            .await
            .unwrap();
        assert_eq!(p1.request_part["schema_version"], 1);
        assert_eq!(p1.request_part["harness"], "claude-code");
        assert_eq!(p1.request_part["session_id"], sid.as_str());
        assert_eq!(p1.model.as_deref(), Some("m"));
        let d1 = delta(&p1);
        assert_eq!(d1["history"]["key"], "messages");
        assert_eq!(d1["history"]["prefix_length"], 0);
        assert_eq!(d1["history"]["append"].as_array().unwrap().len(), 1);
        assert!(
            d1["set"].get("tools").is_some(),
            "big field stored on first turn"
        );
        assert!(d1["set"].get("system").is_some());
        assert_eq!(d1["set"]["model"], "m");
        assert_eq!(d1["remove"], json!([]));
        let state: Value =
            serde_json::from_str(&fs::read_to_string(p1.dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(state["request_body"], body1);

        // Append-only growth: unchanged big fields dedup, small fields verbatim.
        let body2 = json!({
            "model": "m",
            "system": "sys",
            "tools": [{ "name": "t" }],
            "messages": [
                { "role": "user", "content": "a" },
                { "role": "assistant", "content": "b" },
                { "role": "user", "content": "c" },
            ],
        });
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body2)
            .await
            .unwrap();
        let d2 = delta(&p2);
        assert_eq!(d2["history"]["prefix_length"], 1);
        assert_eq!(d2["history"]["append"].as_array().unwrap().len(), 2);
        assert!(
            d2["set"].get("tools").is_none(),
            "unchanged big field omitted"
        );
        assert!(d2["set"].get("system").is_none());
        assert_eq!(d2["set"]["model"], "m", "small field verbatim every turn");

        // Compaction: shorter, diverging history plus a changed big field.
        let body3 = json!({
            "model": "m",
            "system": "sys2",
            "tools": [{ "name": "t" }],
            "messages": [{ "role": "user", "content": "SUMMARY" }],
        });
        let p3 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body3)
            .await
            .unwrap();
        let d3 = delta(&p3);
        assert_eq!(
            d3["history"]["prefix_length"], 0,
            "compaction detected via LCP"
        );
        assert_eq!(d3["history"]["append"].as_array().unwrap().len(), 1);
        assert!(d3["set"].get("tools").is_none());
        assert_eq!(d3["set"]["system"], "sys2", "changed big field re-stored");

        fs::remove_dir_all(vault).unwrap();
    }

    #[tokio::test]
    async fn prepare_capture_remove_list_tracks_dropped_keys() {
        let vault = temp_vault("prep-remove");
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();

        let body1 = json!({
            "model": "m",
            "temperature": 0.5,
            "messages": [{ "role": "user", "content": "a" }],
        });
        prepare_capture(&vault, &adapter, captured(Some(&sid)), body1)
            .await
            .unwrap();

        // Second turn drops both `temperature` and the history key itself:
        // only the former lands in `remove` — history is never a removable key.
        let body2 = json!({ "model": "m" });
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body2)
            .await
            .unwrap();
        let d2 = delta(&p2);
        assert_eq!(d2["remove"], json!(["temperature"]));
        assert_eq!(d2["history"]["prefix_length"], 0);
        assert_eq!(d2["history"]["append"], json!([]));

        fs::remove_dir_all(vault).unwrap();
    }

    #[tokio::test]
    async fn prepare_capture_degenerate_inputs() {
        let vault = temp_vault("prep-degenerate");
        let adapter = claude_adapter();

        // Missing session identity is the only error path.
        let err = match prepare_capture(&vault, &adapter, captured(None), json!({})).await {
            Ok(_) => panic!("missing session identity must be an error"),
            Err(e) => e,
        };
        assert!(
            err.contains("no session identity"),
            "unexpected error: {err}"
        );

        // Missing history key: empty append, prefix 0.
        let sid = uuid::Uuid::new_v4().to_string();
        let p = prepare_capture(
            &vault,
            &adapter,
            captured(Some(&sid)),
            json!({ "model": "m" }),
        )
        .await
        .unwrap();
        let d = delta(&p);
        assert_eq!(d["history"]["prefix_length"], 0);
        assert_eq!(d["history"]["append"], json!([]));
        assert_eq!(d["set"]["model"], "m");

        // Non-object body: empty set/remove, state.json still written verbatim.
        let sid2 = uuid::Uuid::new_v4().to_string();
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&sid2)), json!("nope"))
            .await
            .unwrap();
        let d2 = delta(&p2);
        assert_eq!(d2["set"], json!({}));
        assert_eq!(d2["remove"], json!([]));
        assert_eq!(d2["history"]["append"], json!([]));
        let state: Value =
            serde_json::from_str(&fs::read_to_string(p2.dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(state["request_body"], json!("nope"));

        // Corrupt state.json: treated as no prior — never fatal.
        let sid3 = uuid::Uuid::new_v4().to_string();
        let dir = session_dir(&vault, &sid3).unwrap();
        fs::write(dir.join("state.json"), "{corrupt").unwrap();
        let body = json!({
            "model": "m",
            "tools": [{ "name": "t" }],
            "messages": [{ "role": "user", "content": "a" }],
        });
        let p3 = prepare_capture(&vault, &adapter, captured(Some(&sid3)), body)
            .await
            .unwrap();
        let d3 = delta(&p3);
        assert_eq!(d3["history"]["prefix_length"], 0);
        assert_eq!(d3["history"]["append"].as_array().unwrap().len(), 1);
        assert!(
            d3["set"].get("tools").is_some(),
            "big field stored when prior unreadable"
        );

        fs::remove_dir_all(vault).unwrap();
    }

    #[test]
    fn session_dir_creates_from_meta_without_scanning_and_caches() {
        let vault = temp_vault("capture");
        let session_id = uuid::Uuid::new_v4().to_string();
        let meta_dir = vault.join(".meta");
        fs::create_dir_all(&meta_dir).unwrap();
        fs::create_dir_all(vault.join("2000/01/01").join(&session_id)).unwrap();
        let meta_path = meta_dir.join(format!("{session_id}.json"));
        fs::write(
            &meta_path,
            r#"{"original_start":"2026-07-10T23:30:00-02:00"}"#,
        )
        .unwrap();

        let dir = session_dir(&vault, &session_id).unwrap();
        assert!(dir.ends_with(format!("2026/07/11/{session_id}")));
        assert!(dir.is_dir());
        fs::write(meta_path, r#"{"original_start":"2030-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(session_dir(&vault, &session_id).unwrap(), dir);

        fs::remove_dir_all(vault).unwrap();
    }

    #[test]
    fn update_meta_emits_complete_shape_and_preserves_writer_policy() {
        let vault = temp_vault("meta");
        let session_id = uuid::Uuid::new_v4().to_string();
        let path = vault.join(".meta").join(format!("{session_id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"thread_id":"thread","cwd":"/tmp","git_branch":"main","transcript_path":"/tmp/transcript","model":"old","session_start_source":"native","original_start":"2026-07-10T00:00:00Z"}"#,
        )
        .unwrap();
        let ids = Identity {
            session_id: Some(session_id.clone()),
            ..Default::default()
        };
        let adapter = claude_adapter();

        update_meta(&vault, &adapter, &ids, Some("new")).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 11);
        for key in [
            "schema_version",
            "harness",
            "session_id",
            "thread_id",
            "cwd",
            "git_branch",
            "transcript_path",
            "model",
            "session_start_source",
            "original_start",
            "last_observation",
        ] {
            assert!(object.contains_key(key), "missing {key}");
        }
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["session_id"], session_id);
        assert_eq!(value["thread_id"], "thread");
        assert_eq!(value["model"], "new");
        assert_eq!(value["session_start_source"], "native");
        assert!(serde_json::from_value::<Meta>(value).is_ok());

        fs::remove_dir_all(vault).unwrap();
    }

    // ----- Ordered-persistence (#16) -----

    // HOME (thus the staging root) is process-global; serialize the tests that
    // rewrite it so a temp staging tree can't leak between them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _home: PathBuf,
    }
    fn set_home() -> (HomeGuard, PathBuf) {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_vault("home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        let vault = home.join("vault/sessions");
        fs::create_dir_all(&vault).unwrap();
        (
            HomeGuard {
                _lock: lock,
                _home: home,
            },
            vault,
        )
    }

    fn resp(complete: bool) -> CapturedResponse {
        CapturedResponse {
            status: 200,
            headers: hyper::HeaderMap::new(),
            sse: "event: message_stop\ndata: {\"type\":\"message_stop\"}\n".into(),
            complete,
        }
    }

    fn body(msgs: &[&str]) -> Value {
        let arr: Vec<Value> = msgs
            .iter()
            .map(|m| json!({ "role": "user", "content": m }))
            .collect();
        json!({ "model": "m", "messages": arr })
    }

    fn turns_lines(dir: &Path) -> Vec<Value> {
        fs::read_to_string(dir.join("turns.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn append_content(env: &Value) -> Option<String> {
        env.pointer("/request/body_delta/history/append/0/content")
            .and_then(Value::as_str)
            .map(String::from)
    }

    #[tokio::test]
    async fn reverse_completion_persists_in_preparation_order() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();

        let a = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let b = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a", "b"]))
            .await
            .unwrap();
        let dir = a.dir.clone();

        // Later response (b, seq 1) completes first: staged, but not drained.
        finish_capture(&vault, &adapter, b, &resp(true))
            .await
            .unwrap();
        assert!(
            turns_lines(&dir).is_empty(),
            "nothing persisted behind live gap"
        );
        assert!(has_open_capture(&vault, &sid), "gap keeps session open");

        // Earlier response (a, seq 0) completes: both drain in preparation order.
        finish_capture(&vault, &adapter, a, &resp(true))
            .await
            .unwrap();
        let lines = turns_lines(&dir);
        assert_eq!(lines.len(), 2);
        assert_eq!(append_content(&lines[0]).as_deref(), Some("a"));
        assert_eq!(append_content(&lines[1]).as_deref(), Some("b"));
        assert!(!has_open_capture(&vault, &sid), "fully drained");

        let recon = recon::reconstruct(&dir.join("turns.jsonl")).unwrap();
        assert_eq!(recon.messages[0]["content"], "a");
        assert_eq!(recon.messages[1]["content"], "b");
    }

    #[tokio::test]
    async fn different_sessions_do_not_block_each_other() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let s1 = uuid::Uuid::new_v4().to_string();
        let s2 = uuid::Uuid::new_v4().to_string();
        let p1 = prepare_capture(&vault, &adapter, captured(Some(&s1)), body(&["a"]))
            .await
            .unwrap();
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&s2)), body(&["x"]))
            .await
            .unwrap();
        let (d1, d2) = (p1.dir.clone(), p2.dir.clone());
        finish_capture(&vault, &adapter, p2, &resp(true))
            .await
            .unwrap();
        finish_capture(&vault, &adapter, p1, &resp(true))
            .await
            .unwrap();
        assert_eq!(turns_lines(&d1).len(), 1);
        assert_eq!(turns_lines(&d2).len(), 1);
    }

    #[tokio::test]
    async fn restart_materializes_abandoned_and_interleaves_completed() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();

        let _a = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let b = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a", "b"]))
            .await
            .unwrap();
        let _c = prepare_capture(
            &vault,
            &adapter,
            captured(Some(&sid)),
            body(&["a", "b", "c"]),
        )
        .await
        .unwrap();
        let dir = b.dir.clone();
        // Only seq 1 completes; seq 0 and 2 are abandoned by the "restart".
        finish_capture(&vault, &adapter, b, &resp(true))
            .await
            .unwrap();
        assert!(turns_lines(&dir).is_empty());

        recover_all(&vault).unwrap();

        let lines = turns_lines(&dir);
        assert_eq!(lines.len(), 3, "one record per reserved sequence");
        assert_eq!(
            lines[0]["response"]["complete"],
            json!(false),
            "abandoned seq 0"
        );
        assert_eq!(
            lines[1]["response"]["complete"],
            json!(true),
            "completed seq 1"
        );
        assert_eq!(append_content(&lines[1]).as_deref(), Some("b"));
        assert_eq!(
            lines[2]["response"]["complete"],
            json!(false),
            "abandoned seq 2"
        );
        assert!(
            !has_open_capture(&vault, &sid),
            "journal drained, staging cleared"
        );
    }

    #[tokio::test]
    async fn recovery_is_idempotent_for_an_already_committed_stage() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let p = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let (dir, root, seq) = (p.dir.clone(), p.root.clone(), p.sequence);
        finish_capture(&vault, &adapter, p, &resp(true))
            .await
            .unwrap();
        assert_eq!(turns_lines(&dir).len(), 1);
        let committed = turns_lines(&dir)[0].clone();

        // Simulate a crash AFTER the append but BEFORE journal-retire/stage-delete:
        // recreate the stage and rewind the journal to that sequence.
        let stage = staging_dir(&root, &sid).join(format!(
            "{seq}-{}.json",
            committed["request_id"].as_str().unwrap()
        ));
        atomic_write(
            &stage,
            serde_json::to_string(&json!({
                "root": root, "sequence": seq,
                "request_id": committed["request_id"], "envelope": committed,
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let mut order = read_order(&read_state(&dir));
        order.next_to_drain = seq;
        order.pending.insert(seq, committed.clone());
        persist_state(&dir, read_state(&dir), &order).unwrap();

        recover_all(&vault).unwrap();
        assert_eq!(turns_lines(&dir).len(), 1, "no duplicate envelope");
        assert_eq!(turns_lines(&dir)[0], committed);
    }

    #[tokio::test]
    async fn recovery_repairs_an_exact_prefix_partial_tail() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let p = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let (dir, root, seq) = (p.dir.clone(), p.root.clone(), p.sequence);
        // Build the completed envelope and stage it, but write only a BYTE PREFIX
        // of it to turns.jsonl (a drain that crashed mid-append), no newline.
        let mut env = p.request_part.clone();
        env.as_object_mut().unwrap().insert(
            "response".into(),
            json!({ "status": 200, "headers": {}, "complete": true, "sse": "" }),
        );
        let serialized = serde_json::to_string(&env).unwrap();
        fs::write(dir.join("turns.jsonl"), &serialized[..serialized.len() / 2]).unwrap();
        let stage = staging_dir(&root, &sid).join(format!(
            "{seq}-{}.json",
            env["request_id"].as_str().unwrap()
        ));
        atomic_write(
            &stage,
            serde_json::to_string(&json!({
                "root": root, "sequence": seq, "request_id": env["request_id"], "envelope": env,
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let mut order = read_order(&read_state(&dir));
        order.next_to_drain = seq;
        order.pending.insert(seq, p.request_part.clone());
        persist_state(&dir, read_state(&dir), &order).unwrap();

        recover_all(&vault).unwrap();
        let lines = turns_lines(&dir);
        assert_eq!(lines.len(), 1, "partial tail replaced by the full record");
        assert_eq!(lines[0], env);
    }

    #[tokio::test]
    async fn recovery_fails_closed_on_a_same_id_conflict() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let p = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let (dir, root, seq) = (p.dir.clone(), p.root.clone(), p.sequence);
        let rid = p.request_part["request_id"].as_str().unwrap().to_string();

        // Persisted final line: same request_id, DIFFERENT content, terminated.
        let mut committed = p.request_part.clone();
        committed.as_object_mut().unwrap().insert(
            "response".into(),
            json!({ "status": 200, "complete": true, "sse": "committed" }),
        );
        let committed_text = serde_json::to_string(&committed).unwrap() + "\n";
        fs::write(dir.join("turns.jsonl"), &committed_text).unwrap();

        // Stage: same rid+seq, but a conflicting envelope.
        let mut staged = p.request_part.clone();
        staged.as_object_mut().unwrap().insert(
            "response".into(),
            json!({ "status": 200, "complete": true, "sse": "STAGED-DIFFERENT" }),
        );
        let stage = staging_dir(&root, &sid).join(format!("{seq}-{rid}.json"));
        atomic_write(
            &stage,
            serde_json::to_string(&json!({
                "root": root, "sequence": seq, "request_id": rid, "envelope": staged,
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let mut order = read_order(&read_state(&dir));
        order.next_to_drain = seq;
        order.pending.insert(seq, p.request_part.clone());
        persist_state(&dir, read_state(&dir), &order).unwrap();

        assert!(recover_all(&vault).is_err(), "conflict must fail startup");
        assert_eq!(
            fs::read_to_string(dir.join("turns.jsonl")).unwrap(),
            committed_text,
            "persisted bytes left unchanged on failure"
        );
    }

    #[tokio::test]
    async fn legacy_state_without_ordering_preserves_delta_base() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let dir = session_dir(&vault, &sid).unwrap();
        // A pre-#16 state.json: request_body delta base, no capture_order.
        fs::write(
            dir.join("state.json"),
            serde_json::to_string(&json!({
                "schema_version": 1, "harness": "claude-code",
                "session_id": sid, "request_body": body(&["a"]),
            }))
            .unwrap(),
        )
        .unwrap();

        let p = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a", "b"]))
            .await
            .unwrap();
        // Delta continues from the legacy base (prefix 1, appends "b"), and
        // sequencing initializes lazily at 0.
        let d = &p.request_part["request"]["body_delta"];
        assert_eq!(d["history"]["prefix_length"], 1);
        assert_eq!(d["history"]["append"][0]["content"], "b");
        assert_eq!(p.sequence, 0);
        assert!(has_open_capture(&vault, &sid));
    }

    #[tokio::test]
    async fn sealing_excluded_while_capture_is_open() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let p = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        assert!(
            has_open_capture(&vault, &sid),
            "open reservation blocks sealing"
        );
        finish_capture(&vault, &adapter, p, &resp(true))
            .await
            .unwrap();
        assert!(!has_open_capture(&vault, &sid), "sealable once drained");
    }

    #[tokio::test]
    async fn detachment_rechecks_behind_a_finishing_capture() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let dir = pending.dir.clone();
        let root = pending.root.clone();

        let lock = session_lock(&root, &sid);
        let guard = lock.lock().await;
        let detach_vault = vault.clone();
        let detach_sid = sid.clone();
        let detach_dir = dir.clone();
        let detach = tokio::spawn(async move {
            detach_generation(&detach_vault, &detach_sid, &detach_dir).await
        });
        tokio::task::yield_now().await;
        let finish_vault = vault.clone();
        let finish = tokio::spawn(async move {
            finish_capture(&finish_vault, &claude_adapter(), pending, &resp(true)).await
        });
        tokio::task::yield_now().await;
        drop(guard);

        assert!(
            detach.await.unwrap().unwrap().is_none(),
            "queued detachment must observe the open reservation"
        );
        finish.await.unwrap().unwrap();
        let generation = detach_generation(&vault, &sid, &dir)
            .await
            .unwrap()
            .expect("finished generation detaches");
        assert_eq!(
            recon::reconstruct(&generation.path).unwrap().envelopes,
            1,
            "the concurrent completion remains reconstructable"
        );
    }

    #[test]
    fn recovery_rejects_corrupt_and_wrong_shaped_journals() {
        let (_g, vault) = set_home();
        let sid = uuid::Uuid::new_v4().to_string();
        let dir = vault.join("2026/07/20").join(&sid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("turns.jsonl"), "").unwrap();

        for value in [
            "{".to_string(),
            "[]".to_string(),
            json!({"session_id": sid, "capture_order": "wrong"}).to_string(),
        ] {
            fs::write(dir.join("state.json"), value).unwrap();
            assert!(
                recover_all(&vault).is_err(),
                "invalid persisted state must fail closed"
            );
            assert_eq!(fs::read_to_string(dir.join("turns.jsonl")).unwrap(), "");
        }
    }

    #[test]
    fn recovery_ignores_evidence_from_another_vault_root() {
        let (_g, vault) = set_home();
        let foreign = staging_base()
            .join(sha256_hex(b"/another/canonical/vault"))
            .join("foreign-session")
            .join("0-bad.json");
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::write(&foreign, "not valid current-root evidence").unwrap();

        recover_all(&vault).unwrap();
        assert!(foreign.exists(), "foreign-root evidence remains untouched");
    }

    #[tokio::test]
    async fn recovery_uses_the_discovered_relocated_session_path() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let original = pending.dir;
        let relocated = vault.join("2001/02/03").join(&sid);
        fs::create_dir_all(relocated.parent().unwrap()).unwrap();
        fs::rename(&original, &relocated).unwrap();

        recover_all(&vault).unwrap();
        assert!(
            !original.exists(),
            "recovery must not recreate the cached path"
        );
        assert_eq!(turns_lines(&relocated).len(), 1);
        assert_eq!(
            turns_lines(&relocated)[0]["response"]["complete"],
            json!(false)
        );
    }

    #[tokio::test]
    async fn recovery_requires_stage_root_sequence_and_envelope_identity() {
        let (_g, vault) = set_home();
        for case in ["missing-root", "wrong-root", "missing-request-id"] {
            let adapter = claude_adapter();
            let sid = uuid::Uuid::new_v4().to_string();
            let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
                .await
                .unwrap();
            let mut envelope = pending.request_part.clone();
            envelope
                .as_object_mut()
                .unwrap()
                .insert("response".into(), json!({"complete": true}));
            let request_id = envelope["request_id"].as_str().unwrap().to_string();
            let mut doc = json!({
                "root": pending.root,
                "sequence": pending.sequence,
                "request_id": request_id,
                "envelope": envelope,
            });
            match case {
                "missing-root" => {
                    doc.as_object_mut().unwrap().remove("root");
                }
                "wrong-root" => doc["root"] = json!("/another/vault"),
                "missing-request-id" => {
                    doc.as_object_mut().unwrap().remove("request_id");
                }
                _ => unreachable!(),
            }
            let stage = staging_dir(&canonical_root(&vault), &sid)
                .join(format!("{}-{request_id}.json", pending.sequence));
            atomic_write(&stage, doc.to_string().as_bytes()).unwrap();

            assert!(recover_all(&vault).is_err(), "{case} must fail closed");
            assert!(stage.exists(), "{case} evidence must remain");
            assert!(turns_lines(&pending.dir).is_empty());
            fs::remove_dir_all(staging_dir(&canonical_root(&vault), &sid)).unwrap();
            fs::remove_dir_all(&pending.dir).unwrap();
        }
    }

    #[tokio::test]
    async fn recovery_reconciles_only_matching_retired_stages() {
        let (_g, vault) = set_home();
        for conflict in [false, true] {
            let adapter = claude_adapter();
            let sid = uuid::Uuid::new_v4().to_string();
            let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
                .await
                .unwrap();
            let dir = pending.dir.clone();
            let root = pending.root.clone();
            let seq = pending.sequence;
            finish_capture(&vault, &adapter, pending, &resp(true))
                .await
                .unwrap();
            let committed = turns_lines(&dir)[0].clone();
            let mut staged = committed.clone();
            if conflict {
                staged["response"]["complete"] = json!(false);
            }
            let stage = staging_dir(&root, &sid).join(format!(
                "{seq}-{}.json",
                committed["request_id"].as_str().unwrap()
            ));
            atomic_write(
                &stage,
                json!({
                    "root": root,
                    "sequence": seq,
                    "request_id": committed["request_id"],
                    "envelope": staged,
                })
                .to_string()
                .as_bytes(),
            )
            .unwrap();

            if conflict {
                assert!(recover_all(&vault).is_err());
                assert!(stage.exists(), "conflicting retired evidence is preserved");
            } else {
                recover_all(&vault).unwrap();
                assert!(!stage.exists(), "matching retired evidence is cleaned");
            }
            assert_eq!(turns_lines(&dir), vec![committed], "never duplicate");
        }
    }

    #[tokio::test]
    async fn incomplete_recovery_reconciles_complete_and_prefix_retries() {
        let (_g, vault) = set_home();
        for prefix in [false, true] {
            let adapter = claude_adapter();
            let sid = uuid::Uuid::new_v4().to_string();
            let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
                .await
                .unwrap();
            let dir = pending.dir.clone();
            let mut incomplete = pending.request_part.clone();
            incomplete
                .as_object_mut()
                .unwrap()
                .insert("response".into(), json!({"complete": false}));
            let line = serde_json::to_string(&incomplete).unwrap();
            fs::write(
                dir.join("turns.jsonl"),
                if prefix {
                    line[..line.len() / 2].to_string()
                } else {
                    format!("{line}\n")
                },
            )
            .unwrap();

            recover_all(&vault).unwrap();
            assert_eq!(
                turns_lines(&dir),
                vec![incomplete],
                "retry ends with exactly one incomplete envelope"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_surfaces_cleanup_failure_and_preserves_stage() {
        use std::os::unix::fs::PermissionsExt;

        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let dir = pending.dir.clone();
        let root = pending.root.clone();
        let seq = pending.sequence;
        let mut envelope = pending.request_part.clone();
        envelope
            .as_object_mut()
            .unwrap()
            .insert("response".into(), json!({"complete": true}));
        let stage = staging_dir(&root, &sid).join(format!(
            "{seq}-{}.json",
            envelope["request_id"].as_str().unwrap()
        ));
        atomic_write(
            &stage,
            json!({
                "root": root,
                "sequence": seq,
                "request_id": envelope["request_id"],
                "envelope": envelope,
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        fs::set_permissions(stage.parent().unwrap(), fs::Permissions::from_mode(0o500)).unwrap();

        let error = recover_all(&vault).unwrap_err();
        assert!(error.contains("remove committed stage"), "{error}");
        assert!(stage.exists(), "cleanup failure preserves staged evidence");
        assert_eq!(turns_lines(&dir).len(), 1);

        fs::set_permissions(stage.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        recover_all(&vault).unwrap();
        assert!(!stage.exists());
        assert_eq!(turns_lines(&dir).len(), 1, "retry does not duplicate");
    }
}
