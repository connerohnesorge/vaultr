use std::path::Path;
use std::process::{Command, Output};

fn add_session(root: &Path, sid: &str, harness: &str, ids: &[&str]) {
    let dir = root.join("2026/07/17").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let envelopes = ids
        .iter()
        .map(|id| {
            format!(
                r#"{{"harness":"{harness}","observed_at":"2026-07-17T19:00:00.000Z","response":{{"headers":{{"request-id":"{id}"}}}}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(dir.join("turns.jsonl"), envelopes).unwrap();
    let transcript = root.join(format!("{sid}.transcript.jsonl"));
    let native = if harness == "claude-code" {
        ids.iter()
            .map(|id| {
                format!(
                    r#"{{"type":"assistant","requestId":"{id}","timestamp":"2026-07-17T19:00:00.000Z"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        r#"{"type":"response_item","timestamp":"2026-07-17T19:00:00.000Z"}"#.to_string()
    };
    std::fs::write(&transcript, native + "\n").unwrap();
    std::fs::create_dir_all(root.join(".meta")).unwrap();
    std::fs::write(
        root.join(".meta").join(format!("{sid}.json")),
        format!(
            r#"{{"session_id":"{sid}","original_start":"2026-07-17T19:00:00.000Z","session_start_source":"wire","transcript_path":"{}"}}"#,
            transcript.display()
        ),
    )
    .unwrap();
}

fn coverage(root: &Path, sid: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["sessions", "coverage", sid])
        .env("VAULT_SESSIONS", root)
        .output()
        .unwrap()
}

#[test]
fn coverage_cli_rejects_unsupported_empty_and_all_carryover_denominators() {
    let root = std::env::temp_dir().join(format!("plant-coverage-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let codex = "00000000-0000-4000-8000-000000000028";
    let empty = "00000000-0000-4000-8000-000000000029";
    let complete = "00000000-0000-4000-8000-000000000030";
    let carryover = "00000000-0000-4000-8000-000000000031";
    add_session(&root, codex, "codex", &["req_codex"]);
    add_session(&root, empty, "claude-code", &["req_captured"]);
    std::fs::write(
        root.join(format!("{empty}.transcript.jsonl")),
        "{\"type\":\"user\",\"timestamp\":\"2026-07-17T19:00:00.000Z\"}\n",
    )
    .unwrap();
    add_session(&root, complete, "claude-code", &["req_A", "req_B"]);
    add_session(&root, carryover, "claude-code", &["req_old"]);
    std::fs::write(
        root.join(format!("{carryover}.transcript.jsonl")),
        "{\"type\":\"assistant\",\"requestId\":\"req_old\",\"timestamp\":\"2026-07-17T18:59:59.000Z\"}\n",
    )
    .unwrap();

    let output = coverage(&root, codex);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported for Codex"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("100.0%"));

    let output = coverage(&root, empty);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("no comparable in-window native request IDs"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains('%'));

    let output = coverage(&root, carryover);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("no comparable in-window native request IDs"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains('%'));

    let output = coverage(&root, complete);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("coverage 100.0% (2/2 in-window)"));

    std::fs::remove_dir_all(root).unwrap();
}
