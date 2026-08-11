use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn run(home: &Path, sessions: &Path, key: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_plant"));
    command.args(["agent", "run", "--cli", "codex"]);
    if let Some(key) = key {
        command.args(["--idempotency-key", key]);
    }
    command
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .env("PATH", "")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(b"test prompt")?;
            child.wait_with_output()
        })
        .unwrap()
}

fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let tmp = std::env::temp_dir().join(format!(
        "plant-agent-cli-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let sessions = tmp.join("vault/sessions");
    let bin = home.join(".nix-profile/bin");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    fs::write(
        &herdr,
        "#!/bin/sh\nprintf called >> \"$HOME/herdr-called\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    (tmp, home, sessions)
}

fn result(output: &Output) -> serde_json::Value {
    serde_json::from_str(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .expect("machine-readable Plant result"),
    )
    .unwrap()
}

#[test]
fn agent_cli_error_and_usage_list_pi() {
    let output = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["agent", "run", "--cli", "other"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent run: --cli requires claude|codex|prime|pi"),
        "{stderr}"
    );
    assert!(
        stderr.contains("agent run --cli claude|codex|prime|pi"),
        "{stderr}"
    );
}

#[test]
fn unkeyed_agent_run_preserves_human_status() {
    let (tmp, home, sessions) = fixture("unkeyed");
    let output = run(&home, &sessions, None);
    assert_eq!(output.status.code(), Some(75));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().last(),
        Some("[agent:agent] herdr unavailable")
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"outcome\""));
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn keyed_agent_run_emits_receipts_and_replays_durable_outcomes() {
    let (tmp, home, sessions) = fixture("keyed");
    let retryable = run(&home, &sessions, Some("retryable"));
    assert_eq!(retryable.status.code(), Some(75));
    assert_eq!(result(&retryable)["outcome"], "retryable");
    assert!(result(&retryable).get("durable").is_none());

    let key = "pending";
    let runs = home.join(".local/state/plant/agent-runs");
    fs::create_dir_all(&runs).unwrap();
    fs::write(
        runs.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))),
        format!("{{\"state\":\"in_progress\",\"key\":\"{key}\"}}\n"),
    )
    .unwrap();
    let indeterminate = run(&home, &sessions, Some(key));
    assert_eq!(indeterminate.status.code(), Some(1));
    assert_eq!(result(&indeterminate)["outcome"], "indeterminate");
    assert!(result(&indeterminate).get("durable").is_none());

    let key = "replay";
    fs::write(
        runs.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))),
        format!("{{\"state\":\"succeeded\",\"key\":\"{key}\",\"detail\":\"done once\"}}\n"),
    )
    .unwrap();
    let called = home.join("herdr-called");
    fs::write(&called, "").unwrap();
    let expected = serde_json::json!({
        "outcome": "succeeded",
        "detail": "done once",
    });
    for _ in 0..2 {
        let replayed = run(&home, &sessions, Some(key));
        assert_eq!(replayed.status.code(), Some(0));
        assert_eq!(result(&replayed), expected);
    }
    assert_eq!(fs::read_to_string(called).unwrap(), "");

    fs::remove_dir_all(tmp).unwrap();
}
