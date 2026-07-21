use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use vaultr::{normalize, recon};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn claude_append_reconstructs_and_appends_trailing_response() {
    let r = recon::reconstruct(&fixture("claude_append.jsonl")).unwrap();
    assert_eq!(r.key, "messages");
    assert_eq!(r.harness, Some(recon::Harness::Claude));
    assert_eq!(r.history_len, 4); // system, user, assistant, tool_result-user
    assert_eq!(r.trailing_appended, 1);
    assert_eq!(r.messages.len(), 5);
    let last = &r.messages[4];
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"][0]["text"], "The file says: file contents.");
}

#[test]
fn codex_append_reconstructs_with_trailing_items() {
    let r = recon::reconstruct(&fixture("codex_append.jsonl")).unwrap();
    assert_eq!(r.key, "input");
    assert_eq!(r.harness, Some(recon::Harness::Codex));
    assert_eq!(r.history_len, 5);
    assert_eq!(r.trailing_appended, 1);
    let last = r.messages.last().unwrap();
    assert_eq!(last["type"], "message");
    assert_eq!(last["role"], "assistant");
}

#[test]
fn content_addressed_deltas() {
    let r = recon::reconstruct(&fixture("content_addressed.jsonl")).unwrap();
    assert_eq!(r.key, "input");
    assert_eq!(r.history_len, 4); // order of 4 hashes, dict carried across envelopes
    assert_eq!(r.trailing_appended, 1);
    assert_eq!(r.messages[2]["type"], "function_call");
}

#[test]
fn malformed_lineage_fails_with_location_only() {
    for name in [
        "malformed_prefix.jsonl",
        "non_string_order.jsonl",
        "missing_hash.jsonl",
    ] {
        let err = recon::reconstruct(&fixture(name)).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("reconstruct: raw record 1"), "{message}");
        assert!(!message.contains("CAPTURE_SECRET"), "{message}");
        assert!(!message.contains("opaque-missing-hash"), "{message}");
    }
}

#[test]
fn show_rejects_impossible_lineage_before_printing() {
    let root = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let meta = root.path().join(".meta");
    let session = root.path().join("2026/07/20").join(&id);
    fs::create_dir_all(&meta).unwrap();
    fs::create_dir_all(&session).unwrap();
    fs::write(
        meta.join(format!("{id}.json")),
        r#"{"original_start":"2026-07-20T00:00:00Z"}"#,
    )
    .unwrap();
    fs::copy(
        fixture("malformed_prefix.jsonl"),
        session.join("turns.jsonl"),
    )
    .unwrap();

    for stats in [false, true] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_vaultr"));
        command
            .arg("--vault")
            .arg(root.path())
            .args(["session", "show", &id]);
        if stats {
            command.arg("--stats");
        }
        let output = command.output().unwrap();
        let error = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success());
        assert!(output.stdout.is_empty(), "show printed partial output");
        assert!(error.contains("reconstruct: raw record 1"), "{error}");
        assert!(error.contains("invalid append history lineage"), "{error}");
        assert!(!error.contains("CAPTURE_SECRET"), "{error}");
    }
}

#[test]
fn streaming_visitor_errors_keep_record_location() {
    let err = recon::for_each_envelope(&fixture("claude_append.jsonl"), |_| {
        Err(anyhow::anyhow!("visitor rejected envelope"))
    })
    .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("reconstruct: raw record 1"), "{message}");
    assert!(message.contains("visitor rejected envelope"), "{message}");
}

#[test]
fn compaction_replaces_history() {
    let r = recon::reconstruct(&fixture("compaction.jsonl")).unwrap();
    // prefix_length 0 on the second envelope discards the pre-compaction turns
    assert_eq!(r.history_len, 2);
    assert_eq!(r.trailing_appended, 0); // final response incomplete
    assert_eq!(
        r.messages[0]["content"][0]["text"],
        "Summary of prior conversation."
    );
}

#[test]
fn harness_falls_back_to_input_key_when_envelopes_lack_field() {
    // No `harness` field anywhere: key == "input" resolves Codex.
    let codex = json!({
        "schema_version": 1,
        "request": {"body_delta": {"history": {"key": "input", "prefix_length": 0,
            "append": [{"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}]}]}}},
        "response": {"complete": false}
    });
    let r = recon::reconstruct_reader(format!("{codex}\n").as_bytes()).unwrap();
    assert_eq!(r.harness, Some(recon::Harness::Codex));

    // key == "messages" alone resolves nothing — identity stays undetermined
    // so callers may consult meta.harness as a last resort.
    let claude = json!({
        "schema_version": 1,
        "request": {"body_delta": {"history": {"key": "messages", "prefix_length": 0,
            "append": [{"role": "user", "content": "hi"}]}}},
        "response": {"complete": false}
    });
    let r = recon::reconstruct_reader(format!("{claude}\n").as_bytes()).unwrap();
    assert_eq!(r.harness, None);
}

#[test]
fn harness_envelope_field_outranks_key() {
    // An envelope that says codex is codex even if a delta uses "messages".
    let env = json!({
        "schema_version": 1,
        "harness": "codex",
        "request": {"body_delta": {"history": {"key": "messages", "prefix_length": 0,
            "append": [{"role": "user", "content": "hi"}]}}},
        "response": {"complete": false}
    });
    let r = recon::reconstruct_reader(format!("{env}\n").as_bytes()).unwrap();
    assert_eq!(r.harness, Some(recon::Harness::Codex));
}

#[test]
fn first_explicit_harness_is_authoritative() {
    let envelope = |harness: Option<&str>, key: &str| {
        json!({
            "schema_version": 1,
            "harness": harness,
            "request": {"body_delta": {"history": {
                "key": key, "prefix_length": 0, "append": []
            }}},
            "response": {"complete": false}
        })
    };

    let repeated = format!(
        "{}\n{}\n",
        envelope(Some("claude-code"), "messages"),
        envelope(Some("claude"), "messages")
    );
    assert_eq!(
        recon::reconstruct_reader(repeated.as_bytes())
            .unwrap()
            .harness,
        Some(recon::Harness::Claude)
    );

    let explicit_over_key = format!(
        "{}\n{}\n",
        envelope(None, "input"),
        envelope(Some("claude-code"), "messages")
    );
    assert_eq!(
        recon::reconstruct_reader(explicit_over_key.as_bytes())
            .unwrap()
            .harness,
        Some(recon::Harness::Claude)
    );

    let unknown = format!(
        "{}\n{}\n",
        envelope(Some("unknown"), "messages"),
        envelope(Some("codex"), "input")
    );
    assert_eq!(
        recon::reconstruct_reader(unknown.as_bytes())
            .unwrap()
            .harness,
        Some(recon::Harness::Codex)
    );

    let err = recon::reconstruct(&fixture("conflicting_harness.jsonl")).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("reconstruct: raw record 2"), "{message}");
    assert!(message.contains("conflicting explicit harness labels"));
    assert!(!message.contains("CAPTURE_SECRET"));
}

#[test]
fn raw_and_zst_parity() {
    for name in [
        "claude_append.jsonl",
        "codex_append.jsonl",
        "content_addressed.jsonl",
    ] {
        let raw_path = fixture(name);
        let raw = recon::reconstruct(&raw_path).unwrap();
        let compressed = zstd::encode_all(fs::File::open(&raw_path).unwrap(), 3).unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let zst_path = tmp.path().join("turns.jsonl.zst");
        fs::write(&zst_path, compressed).unwrap();
        let z = recon::reconstruct(&zst_path).unwrap();
        assert_eq!(raw.messages, z.messages, "parity failed for {name}");
        assert_eq!(raw.history_len, z.history_len);
    }
}

#[test]
fn mixed_generations_reconstruct_from_either_sibling() {
    let full = fs::read_to_string(fixture("claude_append.jsonl")).unwrap();
    let mut lines = full.lines();
    let sealed_generation = lines.next().unwrap();
    let raw_generation = lines.next().unwrap();
    assert!(lines.next().is_none());
    assert_eq!(
        serde_json::from_str::<Value>(raw_generation)
            .unwrap()
            .pointer("/request/body_delta/history/prefix_length"),
        Some(&json!(2)),
    );

    let expected = recon::reconstruct(&fixture("claude_append.jsonl")).unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let sealed_path = tmp.path().join("turns.jsonl.zst");
    let raw_path = tmp.path().join("turns.jsonl");
    fs::write(
        &sealed_path,
        zstd::encode_all(format!("{sealed_generation}\n").as_bytes(), 3).unwrap(),
    )
    .unwrap();
    fs::write(&raw_path, format!("{raw_generation}\n")).unwrap();

    let from_raw = recon::reconstruct(&raw_path).unwrap();
    let from_sealed = recon::reconstruct(&sealed_path).unwrap();
    assert_eq!(from_raw.messages, expected.messages);
    assert_eq!(from_sealed.messages, expected.messages);
    assert_eq!(from_raw.envelopes, 2);
    assert_eq!(from_sealed.envelopes, 2);
    for entry in [&raw_path, &sealed_path] {
        let mut seen = 0;
        recon::for_each_envelope(entry, |_| {
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 2, "streaming entry failed for {}", entry.display());
    }

    fs::write(&sealed_path, "not zstd").unwrap();
    assert!(recon::reconstruct(&raw_path).is_err());
}

#[test]
fn detached_generation_reconstructs_once_before_and_after_commit() {
    let full = fs::read_to_string(fixture("claude_append.jsonl")).unwrap();
    let mut lines = full.lines();
    let first = lines.next().unwrap();
    let second = lines.next().unwrap();
    let expected = recon::reconstruct(&fixture("claude_append.jsonl")).unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let sealed = tmp.path().join("turns.jsonl.zst");
    let first_frame = zstd::encode_all(format!("{first}\n").as_bytes(), 3).unwrap();
    fs::write(&sealed, &first_frame).unwrap();
    let detached_body = format!("{second}\n");
    let detached = tmp.path().join(format!(
        "turns.jsonl.sealing-{}-{}",
        first_frame.len(),
        vaultr::vault::sha256_hex(detached_body.as_bytes())
    ));
    fs::write(&detached, &detached_body).unwrap();

    let before = recon::reconstruct(&detached).unwrap();
    assert_eq!(before.envelopes, 2);
    assert_eq!(before.messages, expected.messages);

    let second_frame = zstd::encode_all(format!("{second}\n").as_bytes(), 3).unwrap();
    let mut committed = first_frame;
    committed.extend(second_frame);
    fs::write(&sealed, committed).unwrap();
    let after = recon::reconstruct(&detached).unwrap();
    assert_eq!(
        after.envelopes, 2,
        "detached evidence is not replayed twice"
    );
    assert_eq!(after.messages, expected.messages);
}

#[test]
fn detached_generation_is_not_omitted_for_an_unproven_sealed_suffix() {
    let full = fs::read_to_string(fixture("claude_append.jsonl")).unwrap();
    let mut lines = full.lines();
    let first = format!("{}\n", lines.next().unwrap());
    let second = format!("{}\n", lines.next().unwrap());
    let tmp = tempfile::TempDir::new().unwrap();
    let first_frame = zstd::encode_all(first.as_bytes(), 3).unwrap();
    let mut conflicting = first_frame.clone();
    conflicting.extend(zstd::encode_all(first.as_bytes(), 3).unwrap());
    fs::write(tmp.path().join("turns.jsonl.zst"), conflicting).unwrap();
    let detached = tmp.path().join(format!(
        "turns.jsonl.sealing-{}-{}",
        first_frame.len(),
        vaultr::vault::sha256_hex(second.as_bytes())
    ));
    fs::write(&detached, second).unwrap();

    let error = recon::reconstruct(&detached).unwrap_err().to_string();
    assert!(error.contains("sealed suffix conflicts"), "{error}");
    assert!(!error.contains("Hello there"), "{error}");
}

#[test]
fn detached_generation_errors_do_not_echo_captured_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let captured = "TOP-SECRET-CAPTURED-CONTENT\n";
    let detached = tmp.path().join(format!(
        "turns.jsonl.sealing-0-{}",
        vaultr::vault::sha256_hex(captured.as_bytes())
    ));
    fs::write(&detached, captured).unwrap();
    let error = recon::reconstruct(&detached).unwrap_err().to_string();
    assert!(error.contains("sealed record 1"), "{error}");
    assert!(!error.contains("TOP-SECRET"), "{error}");
}

#[test]
fn permissive_sse_parser_keeps_only_valid_data_json() {
    let events = recon::parse_sse(
        "event: message\n\
         data: {\"n\":1}\n\
         data:   {\"n\":2}  \n\
         data: \n\
         data: [DONE]\n\
         ignored\n\
         data: not-json\n",
    );
    assert_eq!(events, vec![json!({"n": 1}), json!({"n": 2})]);
}

#[test]
fn incomplete_live_tail_ignored() {
    let full = fs::read_to_string(fixture("claude_append.jsonl")).unwrap();
    let complete_lines: Vec<&str> = full.lines().collect();
    // snapshot after only the first envelope + a truncated second line
    let truncated = format!(
        "{}\n{}",
        complete_lines[0],
        &complete_lines[1][..complete_lines[1].len() / 2]
    );
    let r = recon::reconstruct_reader(truncated.as_bytes()).unwrap();
    assert_eq!(r.envelopes, 1);
    assert_eq!(r.history_len, 2); // only the first delta applied
                                  // the first envelope's completed response becomes the trailing message
    assert_eq!(r.trailing_appended, 1);
}

#[test]
fn normalize_excludes_scaffolding_and_strips_reminders() {
    let r = recon::reconstruct(&fixture("claude_append.jsonl")).unwrap();
    let n = normalize::normalize(&r.messages);
    // system message dropped; 4 remain: user, assistant, tool_result-user, trailing assistant
    assert_eq!(n.len(), 4);
    assert_eq!(n[0].role, normalize::Role::User);
    match &n[0].blocks[0] {
        normalize::Block::Text(t) => assert_eq!(t, "Hello there."),
        other => panic!("expected text, got {other:?}"),
    }
    // assistant turn: thinking excluded, text + tool_use kept
    assert_eq!(n[1].blocks.len(), 2);
    assert!(matches!(
        &n[1].blocks[1],
        normalize::Block::ToolUse {
            name,
            correlation_id,
            ..
        } if name == "Read" && correlation_id.as_deref() == Some("t1")
    ));
    assert!(matches!(
        &n[2].blocks[0],
        normalize::Block::ToolResult { correlation_id, .. }
        if correlation_id.as_deref() == Some("t1")
    ));
}

#[test]
fn normalize_codex_items() {
    let r = recon::reconstruct(&fixture("codex_append.jsonl")).unwrap();
    let n = normalize::normalize(&r.messages);
    // developer msg + reasoning dropped: user, tool_use, tool_result, assistant
    assert_eq!(n.len(), 4);
    assert!(
        matches!(&n[1].blocks[0], normalize::Block::ToolUse { name, input, correlation_id }
        if name == "shell"
            && input["cmd"] == Value::String("ls".into())
            && correlation_id.as_deref() == Some("c1"))
    );
    assert!(
        matches!(&n[2].blocks[0], normalize::Block::ToolResult { content, correlation_id }
        if content.contains("a.txt") && correlation_id.as_deref() == Some("c1"))
    );
    assert_eq!(n[3].role, normalize::Role::Assistant);
}

#[test]
fn normalize_preserves_only_string_correlation_ids() {
    let n = normalize::normalize(&[
        json!({"type": "function_call", "name": "shell", "call_id": "", "arguments": {}}),
        json!({"type": "function_call_output", "call_id": 7, "output": "done"}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "claude-id", "name": "Bash", "input": {}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "content": "done"}
        ]}),
    ]);

    assert!(matches!(
        &n[0].blocks[0],
        normalize::Block::ToolUse { correlation_id, .. }
        if correlation_id.as_deref() == Some("")
    ));
    assert!(matches!(
        &n[1].blocks[0],
        normalize::Block::ToolResult { correlation_id, .. }
        if correlation_id.is_none()
    ));
    assert!(matches!(
        &n[2].blocks[0],
        normalize::Block::ToolUse { correlation_id, .. }
        if correlation_id.as_deref() == Some("claude-id")
    ));
    assert!(matches!(
        &n[3].blocks[0],
        normalize::Block::ToolResult { correlation_id, .. }
        if correlation_id.is_none()
    ));
}

#[test]
fn strip_system_reminders_edge_cases() {
    use normalize::strip_system_reminders as strip;
    assert_eq!(strip("a<system-reminder>x</system-reminder>b"), "ab");
    assert_eq!(strip("a<system-reminder>unterminated"), "a");
    assert_eq!(strip("plain"), "plain");
}
