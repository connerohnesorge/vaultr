use super::commit::{capture_tail, CaptureTail, RawGeneration};
use super::*;
use std::io::Write;

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
fn strict_loader_accepts_supported_legacy_and_requires_every_order_field() {
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
    let mut content_addressed_legacy = valid_legacy.clone();
    content_addressed_legacy["schema_version"] = json!(2);
    fs::write(
        dir.join("state.json"),
        serde_json::to_vec(&content_addressed_legacy).unwrap(),
    )
    .unwrap();
    assert!(Journal::load(&dir, sid).unwrap().order.is_none());

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
fn commit_stage_streams_a_small_next_record_after_a_large_previous_record() {
    let dir = test_dir("large-previous");
    let sid = "00000000-0000-4000-8000-000000000048";
    let staged = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &staged);
    let turns = dir.join("turns.jsonl");
    let mut file = fs::File::create(&turns).unwrap();
    file.write_all(br#"{"request_id":"00000000-0000-4000-8000-000000000049","padding":""#)
        .unwrap();
    for _ in 0..256 {
        file.write_all(&[b'x'; 64 * 1024]).unwrap();
    }
    file.write_all(b"\"}\n").unwrap();
    let previous_len = file.metadata().unwrap().len();
    drop(file);
    let stage = stage(&dir, staged.clone());
    let mut journal = Journal::load(&dir, sid).unwrap();

    commit_stage(&mut journal, &stage).unwrap();

    assert_eq!(
        fs::metadata(&turns).unwrap().len(),
        previous_len + serde_json::to_vec(&staged).unwrap().len() as u64 + 1
    );
    let raw = RawGeneration::open(&dir, false).unwrap().unwrap();
    assert!(matches!(
        capture_tail(&raw).unwrap(),
        CaptureTail::ValidTerminated { request_id, .. }
            if request_id == staged["request_id"].as_str().unwrap()
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_skips_trailing_whitespace_records_and_fragments() {
    for (label, trailing) in [
        ("blank-line", b"\n".as_slice()),
        ("space-tab-record", b" \t\n".as_slice()),
        ("unterminated-space-tab", b" \t".as_slice()),
    ] {
        let dir = test_dir(label);
        let sid = "00000000-0000-4000-8000-000000000051";
        let envelope = envelope(sid);
        write_ordered_journal(&dir, sid, "/vault", &envelope);
        let mut before = serde_json::to_vec(&envelope).unwrap();
        before.push(b'\n');
        before.extend_from_slice(trailing);
        fs::write(dir.join("turns.jsonl"), &before).unwrap();
        let stage = stage(&dir, envelope);
        let mut journal = Journal::load(&dir, sid).unwrap();

        commit_stage(&mut journal, &stage).unwrap();

        assert_eq!(fs::read(dir.join("turns.jsonl")).unwrap(), before);
        assert!(!stage.path.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn commit_stage_recovers_the_final_envelope_from_a_concatenated_record() {
    let dir = test_dir("concatenated-tail");
    let sid = "00000000-0000-4000-8000-000000000052";
    let staged = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &staged);
    let mut previous = staged.clone();
    previous["request_id"] = json!("00000000-0000-4000-8000-000000000053");
    let before = [
        serde_json::to_vec(&previous).unwrap(),
        serde_json::to_vec(&staged).unwrap(),
        b"\n\n".to_vec(),
    ]
    .concat();
    fs::write(dir.join("turns.jsonl"), &before).unwrap();
    let stage = stage(&dir, staged);
    let mut journal = Journal::load(&dir, sid).unwrap();

    commit_stage(&mut journal, &stage).unwrap();

    assert_eq!(fs::read(dir.join("turns.jsonl")).unwrap(), before);
    assert!(!stage.path.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_rejects_non_envelope_values_and_residue_without_mutation() {
    for (label, before) in [
        ("empty-object", b"{}\n".as_slice()),
        ("null", b"null\n".as_slice()),
        (
            "invalid-request-id",
            b"{\"request_id\":\"not-a-uuid\"}\n".as_slice(),
        ),
        (
            "residue",
            b"{\"request_id\":\"00000000-0000-4000-8000-000000000053\"}junk\n".as_slice(),
        ),
    ] {
        let dir = test_dir(label);
        let sid = "00000000-0000-4000-8000-000000000054";
        let envelope = envelope(sid);
        write_ordered_journal(&dir, sid, "/vault", &envelope);
        fs::write(dir.join("turns.jsonl"), before).unwrap();
        let stage = stage(&dir, envelope);
        let mut journal = Journal::load(&dir, sid).unwrap();

        assert!(commit_stage(&mut journal, &stage).is_err(), "{label}");

        assert_eq!(fs::read(dir.join("turns.jsonl")).unwrap(), before);
        assert!(stage.path.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn commit_stage_appends_once_after_an_all_blank_file() {
    let dir = test_dir("all-blank");
    let sid = "00000000-0000-4000-8000-000000000055";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    fs::write(dir.join("turns.jsonl"), b"\n \t\n \t").unwrap();
    let first = stage(&dir, envelope.clone());
    let mut journal = Journal::load(&dir, sid).unwrap();

    commit_stage(&mut journal, &first).unwrap();
    let once = fs::read(dir.join("turns.jsonl")).unwrap();
    let retry = stage(&dir, envelope);
    let mut journal = Journal::load(&dir, sid).unwrap();
    commit_stage(&mut journal, &retry).unwrap();

    assert_eq!(fs::read(dir.join("turns.jsonl")).unwrap(), once);
    assert_eq!(
        vaultr::recon::reconstruct(&dir.join("turns.jsonl"))
            .unwrap()
            .envelopes,
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn commit_stage_rejects_terminated_junk_without_mutation() {
    let dir = test_dir("terminated-junk");
    let sid = "00000000-0000-4000-8000-000000000050";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    let before = b"{not-json}\n \t\n".to_vec();
    fs::write(dir.join("turns.jsonl"), &before).unwrap();
    let stage = stage(&dir, envelope);
    let mut journal = Journal::load(&dir, sid).unwrap();

    assert!(commit_stage(&mut journal, &stage).is_err());

    assert_eq!(fs::read(dir.join("turns.jsonl")).unwrap(), before);
    assert!(stage.path.exists());
    assert_eq!(
        Journal::load(&dir, sid)
            .unwrap()
            .require_order()
            .unwrap()
            .next_to_drain,
        0
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
    let serialized = serde_json::to_vec(&envelope).unwrap();
    let mut semantically_equal = vec![b'{', b' '];
    semantically_equal.extend_from_slice(&serialized[1..]);
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

#[cfg(unix)]
#[test]
fn commit_stage_rejects_a_symlinked_raw_without_mutating_any_evidence() {
    use std::os::unix::fs::symlink;

    let root = test_dir("raw-symlink");
    let dir = root.join("session");
    fs::create_dir_all(&dir).unwrap();
    let sid = "00000000-0000-4000-8000-000000000056";
    let envelope = envelope(sid);
    write_ordered_journal(&dir, sid, "/vault", &envelope);
    let stage = stage(&dir, envelope);
    let target = root.join("outside.jsonl");
    fs::write(&target, b"outside evidence\n").unwrap();
    symlink(&target, dir.join("turns.jsonl")).unwrap();
    let target_before = fs::read(&target).unwrap();
    let journal_before = fs::read(dir.join("state.json")).unwrap();
    let stage_before = fs::read(&stage.path).unwrap();
    let mut journal = Journal::load(&dir, sid).unwrap();

    assert!(commit_stage(&mut journal, &stage).is_err());

    assert_eq!(fs::read(&target).unwrap(), target_before);
    assert_eq!(fs::read(dir.join("state.json")).unwrap(), journal_before);
    assert_eq!(fs::read(&stage.path).unwrap(), stage_before);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn raw_handle_keeps_check_and_append_on_one_descriptor() {
    use std::os::unix::fs::symlink;

    let root = test_dir("raw-swap");
    let dir = root.join("session");
    fs::create_dir_all(&dir).unwrap();
    let sid = "00000000-0000-4000-8000-000000000057";
    let first = envelope(sid);
    let mut initial = serde_json::to_vec(&first).unwrap();
    initial.push(b'\n');
    fs::write(dir.join("turns.jsonl"), &initial).unwrap();
    let mut raw = RawGeneration::open(&dir, false).unwrap().unwrap();
    assert!(matches!(
        capture_tail(&raw).unwrap(),
        CaptureTail::ValidTerminated { .. }
    ));
    let retained = dir.join("retained.jsonl");
    fs::rename(dir.join("turns.jsonl"), &retained).unwrap();
    let target = root.join("outside.jsonl");
    fs::write(&target, b"outside evidence\n").unwrap();
    symlink(&target, dir.join("turns.jsonl")).unwrap();

    raw.append_record(b"{\"request_id\":\"00000000-0000-4000-8000-000000000058\"}")
        .unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"outside evidence\n");
    assert!(fs::read(&retained)
        .unwrap()
        .ends_with(b"00000000-0000-4000-8000-000000000058\"}\n"));
    fs::remove_dir_all(root).unwrap();
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

    let root_identity = canonical_root(&vault);
    assert!(sealing_readiness(&root_identity, sid, &dir).is_err());
    assert_eq!(fs::read(&detached).unwrap(), body);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_ignores_an_empty_stage_directory_without_a_journal() {
    let root = test_dir("empty-stage-directory");
    let sessions = root.join("sessions");
    let sid = "00000000-0000-4000-8000-000000000059";
    fs::create_dir_all(sessions.join("2026/07/20").join(sid)).unwrap();
    let root_identity = canonical_root(&sessions);
    let empty_stage = staging_dir(&root_identity, sid);
    fs::create_dir_all(&empty_stage).unwrap();

    recover_all(&sessions).unwrap();

    assert!(empty_stage.exists());
    fs::remove_dir_all(empty_stage.parent().unwrap()).unwrap();
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
