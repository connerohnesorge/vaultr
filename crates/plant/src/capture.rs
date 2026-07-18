//! Vault capture — envelope writer, state.json, meta merge. Delta encoding
//! lives in vaultr::recon next to its inverse.
//! Ports capture/sessionDir/updateMeta (wireproxy.ts:122-422).

use crate::adapter::{Adapter, Identity};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

/// Request-time half: compute delta, write state.json, drop the body Value.
pub fn prepare_capture(
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

    let prior: Value = fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        // take, not clone — the prior body is the full history, often MBs.
        .and_then(|mut v| v.get_mut("request_body").map(Value::take))
        .unwrap_or(Value::Null);
    let body_delta = recon::encode_delta(&prior, &body, adapter.history_key, adapter.big_fields);

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

    let model = body.get("model").and_then(Value::as_str).map(String::from);
    let state = json!({
        "schema_version": 1,
        "harness": adapter.harness,
        "session_id": sid,
        "thread_id": req.ids.thread_id,
        "request_body": body,
    });
    fs::write(
        dir.join("state.json"),
        serde_json::to_string(&state).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| e.to_string())?;

    Ok(PendingCapture {
        dir,
        request_part,
        model,
        req,
    })
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
    writeln!(
        f,
        "{}",
        serde_json::to_string(&pending.request_part).map_err(|e| e.to_string())?
    )
    .map_err(|e| e.to_string())?;
    crate::herdr::maybe_snapshot(vault);
    update_meta(vault, adapter, &pending.req.ids, pending.model.as_deref())
        .map_err(|e| e.to_string())?;
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

    #[test]
    fn prepare_capture_delta_lifecycle() {
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
        let p1 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body1.clone()).unwrap();
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
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body2).unwrap();
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
        let p3 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body3).unwrap();
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

    #[test]
    fn prepare_capture_remove_list_tracks_dropped_keys() {
        let vault = temp_vault("prep-remove");
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();

        let body1 = json!({
            "model": "m",
            "temperature": 0.5,
            "messages": [{ "role": "user", "content": "a" }],
        });
        prepare_capture(&vault, &adapter, captured(Some(&sid)), body1).unwrap();

        // Second turn drops both `temperature` and the history key itself:
        // only the former lands in `remove` — history is never a removable key.
        let body2 = json!({ "model": "m" });
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&sid)), body2).unwrap();
        let d2 = delta(&p2);
        assert_eq!(d2["remove"], json!(["temperature"]));
        assert_eq!(d2["history"]["prefix_length"], 0);
        assert_eq!(d2["history"]["append"], json!([]));

        fs::remove_dir_all(vault).unwrap();
    }

    #[test]
    fn prepare_capture_degenerate_inputs() {
        let vault = temp_vault("prep-degenerate");
        let adapter = claude_adapter();

        // Missing session identity is the only error path.
        let err = match prepare_capture(&vault, &adapter, captured(None), json!({})) {
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
        .unwrap();
        let d = delta(&p);
        assert_eq!(d["history"]["prefix_length"], 0);
        assert_eq!(d["history"]["append"], json!([]));
        assert_eq!(d["set"]["model"], "m");

        // Non-object body: empty set/remove, state.json still written verbatim.
        let sid2 = uuid::Uuid::new_v4().to_string();
        let p2 = prepare_capture(&vault, &adapter, captured(Some(&sid2)), json!("nope")).unwrap();
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
        let p3 = prepare_capture(&vault, &adapter, captured(Some(&sid3)), body).unwrap();
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
}
