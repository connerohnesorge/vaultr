use std::fs;
use std::process::Command;

#[test]
fn index_then_search_returns_a_json_envelope() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("vault/sessions");
    let state = temp.path().join("state");
    let id = "12345678-1234-1234-1234-123456789abc";
    let session = sessions.join("2026/01/01").join(id);
    fs::create_dir_all(sessions.join(".meta")).unwrap();
    fs::create_dir_all(&session).unwrap();
    fs::write(
        sessions.join(".meta").join(format!("{id}.json")),
        r#"{"original_start":"2026-01-01T00:00:00Z","harness":"claude-code"}"#,
    )
    .unwrap();
    fs::write(
        session.join("turns.jsonl"),
        r#"{"harness":"claude-code","request":{"body_delta":{"history":{"key":"messages","prefix_length":0,"append":[{"role":"user","content":"unique search needle"}]}}}}"#,
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_vaultr");
    assert!(Command::new(binary)
        .env("XDG_STATE_HOME", &state)
        .args([
            "--vault",
            sessions.to_str().unwrap(),
            "session",
            "index",
            "--update"
        ])
        .status()
        .unwrap()
        .success());
    let output = Command::new(binary)
        .env("XDG_STATE_HOME", &state)
        .args([
            "--vault",
            sessions.to_str().unwrap(),
            "session",
            "search",
            "unique",
            "needle",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["query"], "unique needle");
    assert_eq!(result["total"], 1);
    assert_eq!(result["hits"][0]["session_id"], id);
}
