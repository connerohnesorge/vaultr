use std::fs;
use tempfile::TempDir;
use vaultr::{recon, vault};

fn fixture_root() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let meta = tmp.path().join(".meta");
    fs::create_dir_all(&meta).unwrap();
    let write = |id: &str, harness: &str, cwd: &str, last: &str| {
        fs::write(
            meta.join(format!("{id}.json")),
            serde_json::json!({
                "schema_version": 1, "harness": harness, "session_id": id,
                "cwd": cwd, "model": "m", "original_start": "2026-07-10T00:00:00.000Z",
                "last_observation": last,
            })
            .to_string(),
        )
        .unwrap();
    };
    write(
        "aaaa1111-0000-0000-0000-000000000001",
        "claude-code",
        "/x",
        "2026-07-12T00:00:00.000Z",
    );
    write(
        "aaaa2222-0000-0000-0000-000000000002",
        "codex",
        "/y",
        "2026-07-14T00:00:00.000Z",
    );
    write(
        "bbbb3333-0000-0000-0000-000000000003",
        "codex",
        "/x",
        "2026-07-13T00:00:00.000Z",
    );
    // session dir for the first one
    let dir = tmp
        .path()
        .join("2026/07/10/aaaa1111-0000-0000-0000-000000000001");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("turns.jsonl"), "").unwrap();
    tmp
}

#[test]
fn list_sorted_newest_first() {
    let tmp = fixture_root();
    let sessions = vault::list_sessions(tmp.path()).unwrap();
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "aaaa2222-0000-0000-0000-000000000002",
            "bbbb3333-0000-0000-0000-000000000003",
            "aaaa1111-0000-0000-0000-000000000001",
        ]
    );
}

#[test]
fn resolve_exact_and_prefix() {
    let tmp = fixture_root();
    let s = vault::resolve_id(tmp.path(), "bbbb").unwrap();
    assert_eq!(s.id, "bbbb3333-0000-0000-0000-000000000003");
    let s = vault::resolve_id(tmp.path(), "aaaa1111-0000-0000-0000-000000000001").unwrap();
    assert_eq!(s.meta.harness.as_deref(), Some("claude-code"));
}

#[test]
fn resolve_ambiguous_lists_candidates() {
    let tmp = fixture_root();
    let err = vault::resolve_id(tmp.path(), "aaaa")
        .unwrap_err()
        .to_string();
    assert!(err.contains("ambiguous"));
    assert!(err.contains("aaaa1111-0000-0000-0000-000000000001"));
    assert!(err.contains("aaaa2222-0000-0000-0000-000000000002"));
}

#[test]
fn resolve_missing_errors() {
    let tmp = fixture_root();
    let err = vault::resolve_id(tmp.path(), "zzzz")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no session matching"));
}

#[test]
fn meta_deserializes_complete_and_legacy_shapes() {
    let complete: vault::Meta = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "harness": "codex",
        "session_id": "session",
        "thread_id": "thread",
        "cwd": "/tmp",
        "git_branch": "main",
        "transcript_path": "/tmp/transcript.jsonl",
        "model": "model",
        "session_start_source": "wire",
        "original_start": "2026-07-10T00:00:00Z",
        "last_observation": "2026-07-10T01:00:00Z"
    }))
    .unwrap();
    assert_eq!(complete.schema_version, Some(1));
    assert_eq!(complete.thread_id.as_deref(), Some("thread"));
    assert_eq!(
        complete.transcript_path.as_deref(),
        Some("/tmp/transcript.jsonl")
    );
    assert_eq!(complete.session_start_source.as_deref(), Some("wire"));

    let legacy: vault::Meta = serde_json::from_str(r#"{"harness":"claude-code"}"#).unwrap();
    assert_eq!(legacy.harness.as_deref(), Some("claude-code"));
    assert!(legacy.schema_version.is_none());
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("thread_id")
        .is_some());
}

#[test]
fn invalid_metadata_defaults_and_filename_remains_authoritative() {
    let tmp = TempDir::new().unwrap();
    let meta = tmp.path().join(".meta");
    fs::create_dir_all(&meta).unwrap();
    fs::write(meta.join("invalid-json.json"), "{").unwrap();
    fs::write(
        meta.join("invalid-type.json"),
        r#"{"schema_version":"one","harness":"codex"}"#,
    )
    .unwrap();
    fs::write(
        meta.join("filename-id.json"),
        r#"{"session_id":"payload-id","harness":"codex","extra":true}"#,
    )
    .unwrap();

    let sessions = vault::list_sessions(tmp.path()).unwrap();
    let invalid_json = sessions.iter().find(|s| s.id == "invalid-json").unwrap();
    assert!(invalid_json.meta.harness.is_none());
    let invalid_type = sessions.iter().find(|s| s.id == "invalid-type").unwrap();
    assert!(invalid_type.meta.harness.is_none());
    let mismatch = sessions.iter().find(|s| s.id == "filename-id").unwrap();
    assert_eq!(mismatch.meta.session_id.as_deref(), Some("payload-id"));
    assert_eq!(
        vault::resolve_id(tmp.path(), "filename-id").unwrap().id,
        "filename-id"
    );
}

#[test]
fn dated_session_dir_normalizes_to_utc_without_io() {
    let tmp = TempDir::new().unwrap();
    let dir =
        vault::dated_session_dir(tmp.path(), "session", Some("2026-07-10T23:30:00-02:00")).unwrap();
    assert!(dir.ends_with("2026/07/11/session"));
    assert!(!tmp.path().join("2026").exists());
    assert!(vault::dated_session_dir(tmp.path(), "session", None).is_none());
    assert!(vault::dated_session_dir(tmp.path(), "session", Some("2026-07-10")).is_none());
}

#[test]
fn session_dir_from_meta_date() {
    let tmp = fixture_root();
    let s = vault::resolve_id(tmp.path(), "aaaa1111").unwrap();
    let dir = vault::session_dir(tmp.path(), &s).unwrap();
    assert!(dir.ends_with("2026/07/10/aaaa1111-0000-0000-0000-000000000001"));
}

#[test]
fn capture_file_is_raw_first_then_zstd() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("turns.jsonl");
    let zst = tmp.path().join("turns.jsonl.zst");

    fs::write(&raw, "").unwrap();
    assert_eq!(vault::capture_file(tmp.path()).unwrap(), raw);
    fs::write(&zst, "").unwrap();
    assert_eq!(vault::capture_file(tmp.path()).unwrap(), raw);
    fs::remove_file(&raw).unwrap();
    assert_eq!(vault::capture_file(tmp.path()).unwrap(), zst);
    fs::remove_file(&zst).unwrap();
    assert!(vault::capture_file(tmp.path()).is_err());
}

#[test]
fn raw_corruption_is_not_hidden_by_zstd_fallback() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("turns.jsonl");
    fs::write(&raw, [0xff, b'\n']).unwrap();
    fs::write(
        tmp.path().join("turns.jsonl.zst"),
        zstd::stream::encode_all("{}\n".as_bytes(), 0).unwrap(),
    )
    .unwrap();

    let selected = vault::capture_file(tmp.path()).unwrap();
    assert_eq!(selected, raw);
    assert!(recon::reconstruct(&selected).is_err());
}

#[test]
fn session_dir_scan_fallback() {
    let tmp = fixture_root();
    // meta says 2026-07-10 but move the dir to another date
    let old = tmp
        .path()
        .join("2026/07/10/aaaa1111-0000-0000-0000-000000000001");
    let new_parent = tmp.path().join("2026/07/11");
    std::fs::create_dir_all(&new_parent).unwrap();
    std::fs::rename(
        &old,
        new_parent.join("aaaa1111-0000-0000-0000-000000000001"),
    )
    .unwrap();
    let s = vault::resolve_id(tmp.path(), "aaaa1111").unwrap();
    let dir = vault::session_dir(tmp.path(), &s).unwrap();
    assert!(dir.ends_with("2026/07/11/aaaa1111-0000-0000-0000-000000000001"));
}
