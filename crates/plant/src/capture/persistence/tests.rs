use super::*;

fn test_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "plant-persistence-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn envelope(sid: &str) -> Value {
    json!({
        "schema_version": 1,
        "request_id": "00000000-0000-4000-8000-000000000040",
        "session_id": sid,
        "request": {"body_delta": {"history": {"key": "messages", "prefix_length": 0, "append": []}}},
        "response": {"complete": true, "sse": "évidence"}
    })
}

fn write_ordered_journal(dir: &Path, sid: &str, root: &str, request: &Value) {
    let mut pending = request.clone();
    pending.as_object_mut().unwrap().remove("response");
    fs::write(
        dir.join("state.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": sid,
            "request_body": {},
            "capture_order": {
                "next_sequence": 1,
                "next_to_drain": 0,
                "pending": {"0": pending},
                "root": root
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn stage(dir: &Path, envelope: Value) -> Stage {
    let path = dir.join("stage/0-00000000-0000-4000-8000-000000000040.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"stage evidence").unwrap();
    Stage {
        path,
        sequence: 0,
        envelope,
    }
}

#[test]
fn strict_loader_accepts_valid_legacy_and_requires_every_order_field() {
    let dir = test_dir("journal-shape");
    let sid = "00000000-0000-4000-8000-000000000041";
    for invalid in [b"{".as_slice(), b"[]".as_slice()] {
        fs::write(dir.join("state.json"), invalid).unwrap();
        assert!(Journal::load(&dir, sid).is_err());
    }
    fs::write(
        dir.join("state.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": sid,
            "request_body": {"messages": []}
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(Journal::load(&dir, sid).unwrap().order.is_none());

    let valid_legacy: Value =
        serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
    for missing in ["schema_version", "harness", "session_id", "request_body"] {
        let mut value = valid_legacy.clone();
        value.as_object_mut().unwrap().remove(missing);
        fs::write(dir.join("state.json"), serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(Journal::load(&dir, sid).is_err(), "missing {missing}");
    }
    let mut invalid_thread = valid_legacy;
    invalid_thread["thread_id"] = json!(42);
    fs::write(
        dir.join("state.json"),
        serde_json::to_vec(&invalid_thread).unwrap(),
    )
    .unwrap();
    assert!(Journal::load(&dir, sid).is_err());

    let request = envelope(sid);
    for missing in ["next_sequence", "next_to_drain", "pending", "root"] {
        write_ordered_journal(&dir, sid, "/vault", &request);
        let mut value: Value =
            serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
        value["capture_order"]
            .as_object_mut()
            .unwrap()
            .remove(missing);
        fs::write(dir.join("state.json"), serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(Journal::load(&dir, sid).is_err(), "missing {missing}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_repairs_a_prefix_split_inside_utf8() {
    let dir = test_dir("utf8-prefix");
    let sid = "00000000-0000-4000-8000-000000000042";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    let serialized = serde_json::to_vec(&envelope).unwrap();
    let split = serialized
        .windows(2)
        .position(|bytes| bytes == "é".as_bytes())
        .unwrap()
        + 1;
    fs::write(dir.join("turns.jsonl"), &serialized[..split]).unwrap();
    let stage = stage(&dir, envelope.clone());
    let mut journal = Journal::load(&dir, sid).unwrap();

    commit_stage(&mut journal, &stage).unwrap();

    assert_eq!(
        fs::read(dir.join("turns.jsonl")).unwrap(),
        [serialized, vec![b'\n']].concat()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_rejects_same_id_content_conflicts_without_mutation() {
    let dir = test_dir("content-conflict");
    let sid = "00000000-0000-4000-8000-000000000045";
    let staged = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &staged);
    let mut committed = staged.clone();
    committed["response"]["sse"] = json!("different");
    let before = [serde_json::to_vec(&committed).unwrap(), vec![b'\n']].concat();
    fs::write(dir.join("turns.jsonl"), &before).unwrap();
    let stage = stage(&dir, staged);
    let mut journal = Journal::load(&dir, sid).unwrap();

    assert!(commit_stage(&mut journal, &stage).is_err());
    assert_eq!(fs::read(dir.join("turns.jsonl")).unwrap(), before);
    assert!(stage.path.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_requires_byte_exact_retired_evidence() {
    let dir = test_dir("byte-conflict");
    let sid = "00000000-0000-4000-8000-000000000046";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    let mut semantically_equal = vec![b' '];
    semantically_equal.extend(serde_json::to_vec(&envelope).unwrap());
    semantically_equal.push(b'\n');
    fs::write(dir.join("turns.jsonl"), &semantically_equal).unwrap();
    let stage = stage(&dir, envelope);
    let mut journal = Journal::load(&dir, sid).unwrap();

    assert!(commit_stage(&mut journal, &stage).is_err());
    assert_eq!(
        fs::read(dir.join("turns.jsonl")).unwrap(),
        semantically_equal
    );
    assert!(stage.path.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_retries_after_append_succeeds_and_journal_persist_fails() {
    let dir = test_dir("journal-retry");
    let sid = "00000000-0000-4000-8000-000000000043";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    let original = fs::read(dir.join("state.json")).unwrap();
    let stage = stage(&dir, envelope.clone());
    let mut journal = Journal::load(&dir, sid).unwrap();
    fs::remove_file(dir.join("state.json")).unwrap();
    fs::create_dir(dir.join("state.json")).unwrap();

    assert!(commit_stage(&mut journal, &stage).is_err());
    assert!(stage.path.exists());
    assert_eq!(
        fs::read_to_string(dir.join("turns.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    fs::remove_dir(dir.join("state.json")).unwrap();
    fs::write(dir.join("state.json"), original).unwrap();
    let mut journal = Journal::load(&dir, sid).unwrap();
    commit_stage(&mut journal, &stage).unwrap();
    assert!(!stage.path.exists());
    assert_eq!(
        fs::read_to_string(dir.join("turns.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn commit_stage_propagates_cleanup_failure_and_retries_exactly_once() {
    use std::os::unix::fs::PermissionsExt;

    let dir = test_dir("cleanup-retry");
    let sid = "00000000-0000-4000-8000-000000000044";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    let stage = stage(&dir, envelope);
    let mut journal = Journal::load(&dir, sid).unwrap();
    fs::set_permissions(
        stage.path.parent().unwrap(),
        fs::Permissions::from_mode(0o500),
    )
    .unwrap();

    assert!(commit_stage(&mut journal, &stage).is_err());
    assert!(stage.path.exists());
    fs::set_permissions(
        stage.path.parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut journal = Journal::load(&dir, sid).unwrap();
    commit_stage(&mut journal, &stage).unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("turns.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn detached_generation_does_not_bypass_strict_journal_loading() {
    let root = test_dir("detached-journal");
    let vault = root.join("sessions");
    let sid = "00000000-0000-4000-8000-000000000047";
    let dir = vault.join("2026/07/20").join(sid);
    fs::create_dir_all(&dir).unwrap();
    let body = b"detached evidence\n";
    let detached = dir.join(format!(
        "turns.jsonl.sealing-0-{}",
        vaultr::vault::sha256_hex(body)
    ));
    fs::write(&detached, body).unwrap();
    fs::write(dir.join("state.json"), b"{corrupt").unwrap();

    assert!(detach_generation(&vault, sid, &dir).await.is_err());
    assert_eq!(fs::read(&detached).unwrap(), body);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn recovery_rejects_symlink_escapes_without_mutating_evidence() {
    use std::os::unix::fs::symlink;

    let root = test_dir("symlink-escape");
    let sessions = root.join("sessions");
    let outside = root.join("outside/07/20/session");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("turns.jsonl"), b"outside evidence\n").unwrap();
    fs::write(
        outside.join("state.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": "session",
            "request_body": {}
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(outside.join("turns.jsonl")).unwrap();

    symlink(root.join("outside"), sessions.join("2026")).unwrap();
    assert!(recover_all(&sessions).is_err());
    assert_eq!(fs::read(outside.join("turns.jsonl")).unwrap(), before);

    fs::remove_file(sessions.join("2026")).unwrap();
    fs::create_dir_all(sessions.join("2026/07/20")).unwrap();
    symlink(&outside, sessions.join("2026/07/20/session")).unwrap();
    assert!(recover_all(&sessions).is_err());
    assert_eq!(fs::read(outside.join("turns.jsonl")).unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}
