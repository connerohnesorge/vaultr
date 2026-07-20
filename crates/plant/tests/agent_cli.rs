use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn run(home: &Path, sessions: &Path, key: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["agent", "run", "--cli", "codex", "--idempotency-key", key])
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
fn agent_run_distinguishes_retryable_and_indeterminate_state() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-agent-cli-{}-{}",
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
    fs::write(&herdr, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();

    let retryable = run(&home, &sessions, "retryable");
    assert_eq!(retryable.status.code(), Some(75));
    assert_eq!(result(&retryable)["state"], "retryable");
    assert_eq!(result(&retryable)["durable"], false);

    let key = "pending";
    let runs = home.join(".local/state/plant/agent-runs");
    fs::create_dir_all(&runs).unwrap();
    fs::write(
        runs.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))),
        format!("{{\"state\":\"in_progress\",\"key\":\"{key}\"}}\n"),
    )
    .unwrap();
    let indeterminate = run(&home, &sessions, key);
    assert_eq!(indeterminate.status.code(), Some(1));
    assert_eq!(result(&indeterminate)["state"], "indeterminate");
    assert_eq!(result(&indeterminate)["durable"], false);

    fs::remove_dir_all(tmp).unwrap();
}
