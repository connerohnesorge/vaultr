//! Vault capture — envelope writer, delta encoding, state.json, meta merge.
//! Ports capture/commonPrefix/sessionDir/updateMeta (wireproxy.ts:122-422).

use crate::adapter::{Adapter, Identity};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

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
    pub request_part: Value, // envelope minus response
    pub model: Option<String>,
    pub req: CapturedRequest,
}

pub struct CapturedResponse {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub sse: String,
    pub complete: bool,
}

static SESSION_DIRS: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

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
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// Parse an ISO timestamp back to SystemTime (meta original_start). Best-effort.
fn parse_iso(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // civil -> days (inverse of above)
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    u64::try_from(secs).ok().map(|s| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s))
}

/// vault/YYYY/MM/DD/<session_id>, dated from meta original_start when parseable.
/// NOTE: TS used local time via Date getters; we use UTC — dates match TS behavior
/// only when the meta timestamp is ISO-UTC, which it always is (iso_now).
pub fn session_dir(vault: &Path, session_id: &str) -> std::io::Result<PathBuf> {
    let key = format!("{}\0{}", vault.display(), session_id);
    let mut guard = SESSION_DIRS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(dir) = map.get(&key) {
        return Ok(dir.clone());
    }
    let mut when = SystemTime::now();
    if let Ok(text) = fs::read_to_string(vault.join(".meta").join(format!("{session_id}.json"))) {
        if let Ok(meta) = serde_json::from_str::<Value>(&text) {
            if let Some(t) = meta.get("original_start").and_then(Value::as_str).and_then(parse_iso) {
                when = t;
            }
        }
    }
    let secs = when.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (y, m, d, ..) = civil(secs);
    let dir = vault.join(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}")).join(session_id);
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

pub fn update_meta(vault: &Path, adapter: &Adapter, ids: &Identity, model: Option<&str>) -> std::io::Result<()> {
    let meta_dir = vault.join(".meta");
    fs::create_dir_all(&meta_dir)?;
    let path = meta_dir.join(format!("{}.json", ids.session_id.as_deref().unwrap_or("unknown")));
    let meta: Value = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let get = |k: &str| meta.get(k).cloned().unwrap_or(Value::Null);
    let now = iso_now();
    let out = json!({
        "schema_version": 1,
        "harness": adapter.harness,
        "session_id": ids.session_id,
        "thread_id": ids.thread_id.clone().map(Value::String).unwrap_or_else(|| get("thread_id")),
        "cwd": get("cwd"),
        "git_branch": get("git_branch"),
        "transcript_path": get("transcript_path"),
        "model": model.map(|m| Value::String(m.to_string())).unwrap_or_else(|| get("model")),
        "session_start_source": if get("session_start_source").is_null() { json!("wire") } else { get("session_start_source") },
        "original_start": if get("original_start").is_null() { json!(now) } else { get("original_start") },
        "last_observation": now,
    });
    fs::write(&path, to_string_pretty_1(&out) + "\n")
}

/// commonPrefix: element-wise JSON equality.
pub fn common_prefix(a: &[Value], b: &[Value]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Request-time half: compute delta, write state.json, drop the body Value.
pub fn prepare_capture(
    vault: &Path,
    adapter: &Adapter,
    req: CapturedRequest,
    body: Value,
) -> Result<PendingCapture, String> {
    let sid = req.ids.session_id.clone().ok_or_else(|| format!("{} request has no session identity", adapter.harness))?;
    let empty = vec![];
    let history = body.get(adapter.history_key).and_then(Value::as_array).unwrap_or(&empty);
    let dir = session_dir(vault, &sid).map_err(|e| e.to_string())?;

    let prior: Value = fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("request_body").cloned())
        .unwrap_or(Value::Null);
    let prior_history = prior.get(adapter.history_key).and_then(Value::as_array).unwrap_or(&empty);
    let prefix = common_prefix(prior_history, history);

    let mut set = Map::new();
    let mut remove: Vec<String> = vec![];
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            if k == adapter.history_key {
                continue;
            }
            if adapter.big_fields.contains(&k.as_str()) {
                if prior.get(k) != Some(v) {
                    set.insert(k.clone(), v.clone());
                }
            } else {
                set.insert(k.clone(), v.clone());
            }
        }
        if let Some(pobj) = prior.as_object() {
            for k in pobj.keys() {
                if !obj.contains_key(k) && k != adapter.history_key {
                    remove.push(k.clone());
                }
            }
        }
    }

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
            "body_delta": {
                "set": set,
                "remove": remove,
                "history": { "key": adapter.history_key, "prefix_length": prefix, "append": history[prefix..] },
            },
        },
    });

    let model = body.get("model").and_then(Value::as_str).map(String::from);
    let state = json!({
        "schema_version": 1,
        "harness": adapter.harness,
        "session_id": sid,
        "thread_id": req.ids.thread_id,
        "request_body": body,
    });
    fs::write(dir.join("state.json"), serde_json::to_string(&state).map_err(|e| e.to_string())? + "\n")
        .map_err(|e| e.to_string())?;

    Ok(PendingCapture { dir, request_part, model, req })
}

/// Stream-end half: attach the response, append the envelope, update meta.
pub fn finish_capture(
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
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending.dir.join("turns.jsonl"))
        .map_err(|e| e.to_string())?;
    writeln!(f, "{}", serde_json::to_string(&pending.request_part).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    crate::herdr::maybe_snapshot(vault);
    update_meta(vault, adapter, &pending.req.ids, pending.model.as_deref()).map_err(|e| e.to_string())?;
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
