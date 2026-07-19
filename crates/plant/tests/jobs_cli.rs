use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn run(home: &Path, sessions: &Path, name: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["jobs", "run", name])
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .output()
        .unwrap()
}

#[test]
fn manual_jobs_propagate_normalized_statuses() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-jobs-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let vault = tmp.join("vault");
    let sessions = vault.join("sessions");
    let jobs = vault.join("jobs");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&jobs).unwrap();

    for (name, script, code) in [
        ("success", "echo ok\nexit 0\n", 0),
        ("retry", "echo later\nexit 75\n", 75),
        ("failure", "echo bad >&2\nexit 9\n", 1),
        ("signal", "kill -TERM $$\n", 1),
    ] {
        fs::write(jobs.join(format!("{name}.1h.sh")), script).unwrap();
        let output = run(&home, &sessions, name);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let ledger = home.join(".local/state/plant/jobs");
    assert!(fs::read_to_string(ledger.join("success.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"success\""));
    assert!(fs::read_to_string(ledger.join("failure.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"failed\""));
    assert!(fs::read_to_string(ledger.join("signal.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"failed\""));
    assert!(!ledger.join("retry.jsonl").exists());

    let missing_home = tmp.join("missing-home");
    fs::write(jobs.join("spawn.1h.sh"), "exit 0\n").unwrap();
    let output = run(&missing_home, &sessions, "spawn");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        fs::read_to_string(missing_home.join(".local/state/plant/jobs/spawn.jsonl"))
            .unwrap()
            .contains("\"detail\":\"spawn:")
    );

    fs::remove_dir_all(tmp).unwrap();
}
