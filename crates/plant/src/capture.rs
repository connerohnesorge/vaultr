//! Vault capture — envelope writer, state.json, meta merge. Delta encoding
//! lives in vaultr::recon next to its inverse.
//! Ports capture/sessionDir/updateMeta (wireproxy.ts:122-422).

mod persistence;

use crate::adapter::{Adapter, Identity};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use vaultr::recon;
use vaultr::vault::{dated_session_dir, Meta};

pub(crate) use persistence::{canonical_root, detach_generation, recover_all, session_lock};

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
    let model = body.get("model").and_then(Value::as_str).map(String::from);
    let (sequence, request_part) = persistence::reserve(&dir, &root, &sid, |prior| {
        let body_delta = recon::encode_delta(prior, &body, adapter.history_key, adapter.big_fields);
        let request = json!({
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
        let state = json!({
            "schema_version": 1,
            "harness": adapter.harness,
            "session_id": sid,
            "thread_id": req.ids.thread_id,
            "request_body": body,
        })
        .as_object()
        .cloned()
        .expect("capture state is an object");
        (request, state)
    })
    .await?;

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
    persistence::commit_completed(
        &pending.dir,
        &pending.root,
        &sid,
        pending.sequence,
        pending.request_part,
        || {
            // Side effects at durable stage acceptance — logged separately,
            // never reclassify an accepted stage as lost.
            crate::herdr::maybe_snapshot(vault);
            if let Err(error) =
                update_meta(vault, adapter, &pending.req.ids, pending.model.as_deref())
            {
                eprintln!("[{}] meta update failed: {error}", adapter.harness);
            }
        },
    )
    .await
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
    use super::persistence::{
        atomic_write, has_open_capture, session_lock, staging_base, staging_dir,
    };
    use super::*;
    use vaultr::vault::sha256_hex;

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

        // Corrupt state.json is evidence, not an empty prior.
        let sid3 = uuid::Uuid::new_v4().to_string();
        let dir = session_dir(&vault, &sid3).unwrap();
        fs::write(dir.join("state.json"), "{corrupt").unwrap();
        let before = fs::read(dir.join("state.json")).unwrap();
        let body = json!({
            "model": "m",
            "tools": [{ "name": "t" }],
            "messages": [{ "role": "user", "content": "a" }],
        });
        assert!(
            prepare_capture(&vault, &adapter, captured(Some(&sid3)), body)
                .await
                .is_err()
        );
        assert_eq!(fs::read(dir.join("state.json")).unwrap(), before);

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
    async fn recovery_removes_atomic_stage_temps_and_materializes_once() {
        let (_g, vault) = set_home();
        for complete in [true, false] {
            let adapter = claude_adapter();
            let sid = uuid::Uuid::new_v4().to_string();
            let pending =
                prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["pending"]))
                    .await
                    .unwrap();
            let request_id = pending.request_part["request_id"].as_str().unwrap();
            let path = staging_dir(&pending.root, &sid).join(format!(
                "{}-{request_id}.tmp-{}",
                pending.sequence,
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = if complete {
                let mut envelope = pending.request_part.clone();
                envelope
                    .as_object_mut()
                    .unwrap()
                    .insert("response".into(), json!({"complete": true}));
                serde_json::to_vec(&json!({
                    "root": pending.root,
                    "sequence": pending.sequence,
                    "request_id": request_id,
                    "envelope": envelope,
                }))
                .unwrap()
            } else {
                b"{\"root\":".to_vec()
            };
            fs::write(&path, bytes).unwrap();

            recover_all(&vault).unwrap();

            assert!(!path.exists(), "atomic temp debris removed");
            let lines = turns_lines(&pending.dir);
            assert_eq!(lines.len(), 1, "one incomplete Envelope");
            assert_eq!(lines[0]["request_id"], request_id);
            assert_eq!(lines[0]["response"]["complete"], json!(false));
        }
    }

    #[tokio::test]
    async fn recovery_rejects_near_miss_atomic_stage_temp_names() {
        let (_g, vault) = set_home();
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["pending"]))
            .await
            .unwrap();
        let request_id = pending.request_part["request_id"].as_str().unwrap();
        let path = staging_dir(&pending.root, &sid).join(format!(
            "{}-{request_id}.tmp-{}-extra",
            pending.sequence,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"debris").unwrap();
        let journal_before = fs::read(pending.dir.join("state.json")).unwrap();

        assert!(recover_all(&vault).is_err());

        assert!(path.exists(), "unrecognized evidence remains fail-closed");
        assert_eq!(
            fs::read(pending.dir.join("state.json")).unwrap(),
            journal_before
        );
        assert!(turns_lines(&pending.dir).is_empty());
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
}
