//! Vault capture: envelope construction, private persistence, and immutable
//! generation maintenance. Delta encoding lives in vaultr::recon next to its
//! inverse. Sweep and Herdr enter capture-owned persistence APIs.

mod generation;
mod persistence;
mod session_fs;

use crate::adapter::{Adapter, Identity};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use vaultr::recon;
use vaultr::vault::{contained_levels, dated_session_dir, Meta};

#[cfg(test)]
use std::cell::Cell;

pub(crate) use generation::{
    append_herdr_snapshot, current_herdr_snapshot, scrub, seal_ready_generation,
};
use persistence::canonical_root;

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
    pub request_part: Value,
    pub model: Option<String>,
    pub req: CapturedRequest,
    _active: persistence::ActiveCapture,
}

pub struct CapturedResponse {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub sse: String,
    pub complete: bool,
}

static SESSION_DIRS: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static RECOVERY_CALLS: Cell<usize> = const { Cell::new(0) };
}

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

pub(crate) fn recover_all(vault: &Path) -> Result<(), String> {
    #[cfg(test)]
    RECOVERY_CALLS.with(|calls| calls.set(calls.get() + 1));
    persistence::recover_all(vault)
}

pub(crate) fn recover_live(vault: &Path, min_age: std::time::Duration) -> Result<(), String> {
    #[cfg(test)]
    RECOVERY_CALLS.with(|calls| calls.set(calls.get() + 1));
    persistence::recover_live(vault, min_age)
}

#[cfg(test)]
pub(crate) fn reset_recovery_calls() {
    RECOVERY_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn recovery_calls() -> usize {
    RECOVERY_CALLS.with(Cell::get)
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
    // Refuse a symlinked date level rather than writing captures through it —
    // the same containment the vaultr reader enforces on this exact path.
    contained_levels(vault, &dir).map_err(std::io::Error::other)?;
    fs::create_dir_all(&dir)?;
    // Re-check after creation: nothing we just made may be a symlink, and a
    // level swapped underneath us between the two walks is caught here.
    contained_levels(vault, &dir).map_err(std::io::Error::other)?;
    map.insert(key, dir.clone());
    Ok(dir)
}

fn allowed_headers(headers: &hyper::HeaderMap) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in headers {
        let name = key.as_str().to_ascii_lowercase();
        let allowed = matches!(
            name.as_str(),
            "content-type" | "request-id" | "x-request-id" | "retry-after"
        ) || name.starts_with("anthropic-ratelimit-")
            || name.starts_with("x-ratelimit-");
        if !allowed {
            continue;
        }
        if let Ok(text) = value.to_str() {
            out.insert(name, Value::String(text.to_string()));
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
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let now = iso_now();
    meta.schema_version = Some(1);
    meta.harness = Some(adapter.harness.capture_label().to_string());
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

/// Drops this process could not even record in `.meta`. Reported on /health.
static UNRECORDED_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Drops this process recorded in `.meta`. Reported on /health.
static RECORDED_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn unrecorded_drops() -> u64 {
    UNRECORDED_DROPS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn recorded_drops() -> u64 {
    RECORDED_DROPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Account for one observed turn that capture failed to persist. The marker is
/// orders of magnitude smaller than the Envelope, so it survives the storage
/// pressure that usually causes the drop. When even it fails, count in memory.
pub fn record_drop(vault: &Path, session_id: &str, reason: &str) {
    match write_drop(vault, session_id, reason) {
        Ok(()) => {
            RECORDED_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Err(error) => {
            UNRECORDED_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[capture] dropped turn for {session_id} unrecorded: {error}");
        }
    }
}

fn write_drop(vault: &Path, session_id: &str, reason: &str) -> std::io::Result<()> {
    let meta_dir = vault.join(".meta");
    fs::create_dir_all(&meta_dir)?;
    let path = meta_dir.join(format!("{session_id}.json"));
    let mut meta: Meta = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let now = iso_now();
    meta.session_id
        .get_or_insert_with(|| session_id.to_string());
    meta.dropped_turns += 1;
    meta.first_drop.get_or_insert_with(|| now.clone());
    meta.last_drop = Some(now);
    meta.last_drop_reason = Some(reason.to_string());
    crate::state::replace_file(
        &path,
        (to_string_pretty_1(&json!(meta)) + "\n").as_bytes(),
        crate::state::Durability::Rename,
    )
}

/// The preflight decision. An unmeasurable volume is never treated as full.
fn headroom_shortfall(free: Option<u64>, floor: u64) -> Option<String> {
    free.filter(|free| *free < floor)
        .map(|free| format!("storage headroom {free} below floor {floor}"))
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
    let free = crate::fsutil::free_bytes(vault);
    prepare_capture_with_free(vault, adapter, req, body, free).await
}

pub async fn prepare_capture_offloaded(
    vault: PathBuf,
    adapter: Adapter,
    req: CapturedRequest,
    body: Value,
) -> Result<PendingCapture, String> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(prepare_capture(&vault, &adapter, req, body))
    })
    .await
    .map_err(|error| format!("capture preparation task failed: {error}"))?
}

async fn prepare_capture_with_free(
    vault: &Path,
    adapter: &Adapter,
    req: CapturedRequest,
    body: Value,
    free: Option<u64>,
) -> Result<PendingCapture, String> {
    let label = adapter.harness.capture_label();
    let sid = req.ids.session_id.clone().ok_or_else(|| {
        UNRECORDED_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{label} request has no session identity")
    })?;
    // Preflight: a failed multi-MB state.json write consumes the free blocks the
    // small drop marker needs, so skip it and record the gap while it still fits.
    let floor = crate::fsutil::headroom_floor();
    if let Some(reason) = headroom_shortfall(free, floor) {
        record_drop(vault, &sid, &reason);
        return Err(reason);
    }
    let dir = session_dir(vault, &sid).map_err(|error| error.to_string())?;
    let root = canonical_root(vault);
    let model = body.get("model").and_then(Value::as_str).map(String::from);
    let (sequence, request_part, active) = persistence::reserve(&dir, &root, &sid, |prior| {
        let body_delta = recon::encode_delta(prior, &body, adapter.history_key, adapter.big_fields);
        let request = json!({
            "schema_version": 1,
            "request_id": uuid::Uuid::new_v4().to_string(),
            "observed_at": iso_now(),
            "harness": label,
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
        let state = json!({
            "schema_version": 1,
            "harness": label,
            "session_id": sid,
            "thread_id": req.ids.thread_id,
            "request_body": body,
        })
        .as_object()
        .cloned()
        .expect("capture state is an object");
        (request, state)
    })
    .await
    .inspect_err(|error| record_drop(vault, &sid, error))?;

    Ok(PendingCapture {
        dir,
        root,
        sequence,
        request_part,
        model,
        req,
        _active: active,
    })
}

/// Stream-end half: attach the response, durably STAGE the completed Envelope
/// (finish succeeds here even if an earlier live sequence blocks draining), run
/// the Session Index / Herdr side effects at durable stage acceptance, then
/// drain every eligible sequence into `turns.jsonl` in preparation order.
pub async fn finish_capture_offloaded(
    vault: PathBuf,
    adapter: Adapter,
    pending: PendingCapture,
    response: CapturedResponse,
) -> Result<(), String> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(finish_capture(&vault, &adapter, pending, &response))
    })
    .await
    .map_err(|error| format!("capture completion task failed: {error}"))?
}

pub async fn finish_capture(
    vault: &Path,
    adapter: &Adapter,
    mut pending: PendingCapture,
    response: &CapturedResponse,
) -> Result<(), String> {
    if let Some(object) = pending.request_part.as_object_mut() {
        object.insert(
            "response".into(),
            json!({
                "status": response.status,
                "headers": allowed_headers(&response.headers),
                "complete": response.complete,
                "sse": response.sse,
            }),
        );
    }
    let sid = pending
        .request_part
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let sequence = pending.sequence;
    persistence::commit_completed(
        &pending.dir,
        &pending.root,
        &sid,
        pending.sequence,
        pending.request_part,
        || {
            crate::herdr::maybe_snapshot(vault);
            if let Err(error) =
                update_meta(vault, adapter, &pending.req.ids, pending.model.as_deref())
            {
                eprintln!(
                    "[{}] meta update failed: {error}",
                    adapter.harness.capture_label()
                );
            }
        },
    )
    .await
    .inspect_err(|error| record_drop(vault, &sid, &format!("sequence {sequence}: {error}")))
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
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("json serialize");
    String::from_utf8(buffer).expect("utf8")
}

#[cfg(test)]
mod tests;
