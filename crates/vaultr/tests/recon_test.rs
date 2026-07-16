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
