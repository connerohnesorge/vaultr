use std::fs;
use tempfile::TempDir;
use vaultr::vault;

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
fn session_dir_from_meta_date_and_capture_file() {
    let tmp = fixture_root();
    let s = vault::resolve_id(tmp.path(), "aaaa1111").unwrap();
    let dir = vault::session_dir(tmp.path(), &s).unwrap();
    assert!(dir.ends_with("2026/07/10/aaaa1111-0000-0000-0000-000000000001"));
    let file = vault::capture_file(&dir).unwrap();
    assert!(file.ends_with("turns.jsonl"));
    // zst fallback
    std::fs::remove_file(&file).unwrap();
    std::fs::write(dir.join("turns.jsonl.zst"), "").unwrap();
    assert!(vault::capture_file(&dir)
        .unwrap()
        .ends_with("turns.jsonl.zst"));
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
