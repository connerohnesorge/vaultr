use super::persistence::{has_open_capture, session_lock, staging_base, staging_dir};
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

    let error = match prepare_capture(&vault, &adapter, captured(None), json!({})).await {
        Ok(_) => panic!("missing session identity must be an error"),
        Err(error) => error,
    };
    assert!(
        error.contains("no session identity"),
        "unexpected error: {error}"
    );

    let sid = uuid::Uuid::new_v4().to_string();
    let pending = prepare_capture(
        &vault,
        &adapter,
        captured(Some(&sid)),
        json!({ "model": "m" }),
    )
    .await
    .unwrap();
    let body_delta = delta(&pending);
    assert_eq!(body_delta["history"]["prefix_length"], 0);
    assert_eq!(body_delta["history"]["append"], json!([]));
    assert_eq!(body_delta["set"]["model"], "m");

    let sid2 = uuid::Uuid::new_v4().to_string();
    let pending2 = prepare_capture(&vault, &adapter, captured(Some(&sid2)), json!("nope"))
        .await
        .unwrap();
    let body_delta2 = delta(&pending2);
    assert_eq!(body_delta2["set"], json!({}));
    assert_eq!(body_delta2["remove"], json!([]));
    assert_eq!(body_delta2["history"]["append"], json!([]));
    let state: Value =
        serde_json::from_str(&fs::read_to_string(pending2.dir.join("state.json")).unwrap())
            .unwrap();
    assert_eq!(state["request_body"], json!("nope"));

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

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct HomeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    _home: PathBuf,
}

fn set_home() -> (HomeGuard, PathBuf) {
    let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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

fn response(complete: bool) -> CapturedResponse {
    CapturedResponse {
        status: 200,
        headers: hyper::HeaderMap::new(),
        sse: "event: message_stop\ndata: {\"type\":\"message_stop\"}\n".into(),
        complete,
    }
}

fn body(messages: &[&str]) -> Value {
    let messages = messages
        .iter()
        .map(|message| json!({ "role": "user", "content": message }))
        .collect::<Vec<Value>>();
    json!({ "model": "m", "messages": messages })
}

fn turns_lines(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join("turns.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn append_content(envelope: &Value) -> Option<String> {
    envelope
        .pointer("/request/body_delta/history/append/0/content")
        .and_then(Value::as_str)
        .map(String::from)
}

#[tokio::test]
async fn reverse_completion_persists_in_preparation_order() {
    let (_guard, vault) = set_home();
    let adapter = claude_adapter();
    let sid = uuid::Uuid::new_v4().to_string();

    let first = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
        .await
        .unwrap();
    let second = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a", "b"]))
        .await
        .unwrap();
    let dir = first.dir.clone();

    finish_capture(&vault, &adapter, second, &response(true))
        .await
        .unwrap();
    assert!(
        turns_lines(&dir).is_empty(),
        "nothing persisted behind live gap"
    );
    assert!(has_open_capture(&vault, &sid), "gap keeps session open");

    finish_capture(&vault, &adapter, first, &response(true))
        .await
        .unwrap();
    let lines = turns_lines(&dir);
    assert_eq!(lines.len(), 2);
    assert_eq!(append_content(&lines[0]).as_deref(), Some("a"));
    assert_eq!(append_content(&lines[1]).as_deref(), Some("b"));
    assert!(!has_open_capture(&vault, &sid), "fully drained");

    let reconstructed = recon::reconstruct(&dir.join("turns.jsonl")).unwrap();
    assert_eq!(reconstructed.messages[0]["content"], "a");
    assert_eq!(reconstructed.messages[1]["content"], "b");
}

#[tokio::test]
async fn different_sessions_do_not_block_each_other() {
    let (_guard, vault) = set_home();
    let adapter = claude_adapter();
    let first_sid = uuid::Uuid::new_v4().to_string();
    let second_sid = uuid::Uuid::new_v4().to_string();
    let first = prepare_capture(&vault, &adapter, captured(Some(&first_sid)), body(&["a"]))
        .await
        .unwrap();
    let second = prepare_capture(&vault, &adapter, captured(Some(&second_sid)), body(&["x"]))
        .await
        .unwrap();
    let first_dir = first.dir.clone();
    let second_dir = second.dir.clone();
    finish_capture(&vault, &adapter, second, &response(true))
        .await
        .unwrap();
    finish_capture(&vault, &adapter, first, &response(true))
        .await
        .unwrap();
    assert_eq!(turns_lines(&first_dir).len(), 1);
    assert_eq!(turns_lines(&second_dir).len(), 1);
}

#[tokio::test]
async fn restart_materializes_abandoned_and_interleaves_completed() {
    let (_guard, vault) = set_home();
    let adapter = claude_adapter();
    let sid = uuid::Uuid::new_v4().to_string();

    let _first = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
        .await
        .unwrap();
    let second = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a", "b"]))
        .await
        .unwrap();
    let _third = prepare_capture(
        &vault,
        &adapter,
        captured(Some(&sid)),
        body(&["a", "b", "c"]),
    )
    .await
    .unwrap();
    let dir = second.dir.clone();
    finish_capture(&vault, &adapter, second, &response(true))
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
    let (_guard, vault) = set_home();
    for complete in [true, false] {
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["pending"]))
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
    let (_guard, vault) = set_home();
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
    let (_guard, vault) = set_home();
    let adapter = claude_adapter();
    let sid = uuid::Uuid::new_v4().to_string();
    let dir = session_dir(&vault, &sid).unwrap();
    fs::write(
        dir.join("state.json"),
        serde_json::to_string(&json!({
            "schema_version": 1, "harness": "claude-code",
            "session_id": sid, "request_body": body(&["a"]),
        }))
        .unwrap(),
    )
    .unwrap();

    let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a", "b"]))
        .await
        .unwrap();
    let body_delta = &pending.request_part["request"]["body_delta"];
    assert_eq!(body_delta["history"]["prefix_length"], 1);
    assert_eq!(body_delta["history"]["append"][0]["content"], "b");
    assert_eq!(pending.sequence, 0);
    assert!(has_open_capture(&vault, &sid));
}

#[tokio::test]
async fn detachment_rechecks_behind_a_finishing_capture() {
    if !crate::process::which("zstd") {
        return;
    }
    let (_guard, vault) = set_home();
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
        seal_ready_generation(&detach_vault, &detach_sid, &detach_dir).await
    });
    tokio::task::yield_now().await;
    let finish_vault = vault.clone();
    let finish = tokio::spawn(async move {
        finish_capture(&finish_vault, &claude_adapter(), pending, &response(true)).await
    });
    tokio::task::yield_now().await;
    drop(guard);

    assert!(
        detach.await.unwrap().unwrap().is_none(),
        "queued detachment must observe the open reservation"
    );
    finish.await.unwrap().unwrap();
    let generation = seal_ready_generation(&vault, &sid, &dir)
        .await
        .unwrap()
        .expect("finished generation seals");
    assert_eq!(
        recon::reconstruct(&generation.path).unwrap().envelopes,
        1,
        "the concurrent completion remains reconstructable"
    );
}

#[test]
fn recovery_ignores_evidence_from_another_vault_root() {
    let (_guard, vault) = set_home();
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
    let (_guard, vault) = set_home();
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
    let (_guard, vault) = set_home();
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
        let mut document = json!({
            "root": pending.root,
            "sequence": pending.sequence,
            "request_id": request_id,
            "envelope": envelope,
        });
        match case {
            "missing-root" => {
                document.as_object_mut().unwrap().remove("root");
            }
            "wrong-root" => document["root"] = json!("/another/vault"),
            "missing-request-id" => {
                document.as_object_mut().unwrap().remove("request_id");
            }
            _ => unreachable!(),
        }
        let stage = staging_dir(&canonical_root(&vault), &sid)
            .join(format!("{}-{request_id}.json", pending.sequence));
        crate::fsutil::atomic_replace(&stage, document.to_string().as_bytes()).unwrap();

        assert!(recover_all(&vault).is_err(), "{case} must fail closed");
        assert!(stage.exists(), "{case} evidence must remain");
        assert!(turns_lines(&pending.dir).is_empty());
        fs::remove_dir_all(staging_dir(&canonical_root(&vault), &sid)).unwrap();
        fs::remove_dir_all(&pending.dir).unwrap();
    }
}

#[tokio::test]
async fn recovery_reconciles_only_matching_retired_stages() {
    let (_guard, vault) = set_home();
    for conflict in [false, true] {
        let adapter = claude_adapter();
        let sid = uuid::Uuid::new_v4().to_string();
        let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
            .await
            .unwrap();
        let dir = pending.dir.clone();
        let root = pending.root.clone();
        let sequence = pending.sequence;
        finish_capture(&vault, &adapter, pending, &response(true))
            .await
            .unwrap();
        let committed = turns_lines(&dir)[0].clone();
        let mut staged = committed.clone();
        if conflict {
            staged["response"]["complete"] = json!(false);
        }
        let stage = staging_dir(&root, &sid).join(format!(
            "{sequence}-{}.json",
            committed["request_id"].as_str().unwrap()
        ));
        crate::fsutil::atomic_replace(
            &stage,
            json!({
                "root": root,
                "sequence": sequence,
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
    let (_guard, vault) = set_home();
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

#[tokio::test]
async fn incomplete_recovery_rejects_conflicting_capture_without_mutation() {
    let (_guard, vault) = set_home();
    let adapter = claude_adapter();
    let sid = uuid::Uuid::new_v4().to_string();
    let pending = prepare_capture(&vault, &adapter, captured(Some(&sid)), body(&["a"]))
        .await
        .unwrap();
    let turns = pending.dir.join("turns.jsonl");
    let state = pending.dir.join("state.json");
    let mut conflicting = pending.request_part.clone();
    conflicting
        .as_object_mut()
        .unwrap()
        .insert("response".into(), json!({"complete": true}));
    let capture_before = serde_json::to_vec(&conflicting).unwrap();
    fs::write(&turns, [&capture_before[..], b"\n"].concat()).unwrap();
    let journal_before = fs::read(&state).unwrap();

    assert!(recover_all(&vault).is_err());
    assert_eq!(
        fs::read(&turns).unwrap(),
        [&capture_before[..], b"\n"].concat()
    );
    assert_eq!(fs::read(&state).unwrap(), journal_before);
}
