use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn write_job(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

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
        ("success", "#!/bin/sh\necho ok\nexit 0\n", 0),
        ("retry", "#!/bin/sh\necho later\nexit 75\n", 75),
        ("failure", "#!/bin/sh\necho bad >&2\nexit 9\n", 1),
        ("signal", "#!/bin/sh\nkill -TERM $$\n", 1),
    ] {
        write_job(&jobs.join(format!("{name}.1h.sh")), script);
        let output = run(&home, &sessions, name);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // polyglot: extension is ignored, the shebang picks the interpreter
    write_job(&jobs.join("tsjob.1h.ts"), "#!/bin/sh\necho ts ok\nexit 0\n");
    let output = run(&home, &sessions, "tsjob");
    assert_eq!(
        output.status.code(),
        Some(0),
        "tsjob: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // executable but no shebang: the OS exec fallback runs it via /bin/sh,
    // so legacy shebang-less scripts keep working
    write_job(&jobs.join("noshebang.1h.sh"), "echo hi\nexit 0\n");
    let output = run(&home, &sessions, "noshebang");
    assert_eq!(output.status.code(), Some(0));

    // Exact-name compression discovery is typed for scheduler dispatch only.
    // A manual jobs run must still execute the wrapper through the same fence.
    write_job(
        &jobs.join("compress.30m.sh"),
        "#!/bin/sh\ntouch \"$HOME/compress-wrapper-called\"\n",
    );
    let output = run(&home, &sessions, "compress");
    assert_eq!(output.status.code(), Some(0));
    assert!(home.join("compress-wrapper-called").exists());

    // not executable at all: spawn fails and is recorded
    fs::write(jobs.join("nonexec.1h.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let output = run(&home, &sessions, "nonexec");
    assert_eq!(output.status.code(), Some(1));

    let ledger = home.join(".local/state/plant/jobs");
    assert!(fs::read_to_string(ledger.join("success.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"success\""));
    let success: serde_json::Value = serde_json::from_str(
        fs::read_to_string(ledger.join("success.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert!(
        success
            .get("attempt_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "every durable record is tied to its pre-dispatch fence"
    );
    assert!(fs::read_to_string(ledger.join("failure.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"failed\""));
    assert!(fs::read_to_string(ledger.join("signal.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"failed\""));
    assert!(!ledger.join("retry.jsonl").exists());
    assert!(fs::read_to_string(ledger.join("tsjob.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"success\""));
    assert!(fs::read_to_string(ledger.join("noshebang.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"success\""));
    assert!(fs::read_to_string(ledger.join("nonexec.jsonl"))
        .unwrap()
        .contains("\"detail\":\"spawn:"));

    let retry_again = run(&home, &sessions, "retry");
    assert_eq!(retry_again.status.code(), Some(75));
    assert!(!ledger.join("retry.jsonl").exists());
    assert!(
        fs::read_to_string(home.join(".local/state/plant/job-attempts/retry.json"))
            .unwrap()
            .contains("\"retryable\":true")
    );

    let missing_home = tmp.join("missing-home");
    write_job(&jobs.join("spawn.1h.sh"), "#!/bin/sh\nexit 0\n");
    let output = run(&missing_home, &sessions, "spawn");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        fs::read_to_string(missing_home.join(".local/state/plant/jobs/spawn.jsonl"))
            .unwrap()
            .contains("\"detail\":\"spawn:")
    );

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn unavailable_ledger_prevents_job_side_effects() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-jobs-ledger-{}-{}",
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
    fs::create_dir_all(home.join(".local/state/plant")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    fs::write(home.join(".local/state/plant/jobs"), "not a directory").unwrap();
    write_job(
        &jobs.join("blocked.1h.sh"),
        "#!/bin/sh\ntouch \"$HOME/side-effect\"\n",
    );

    let output = run(&home, &sessions, "blocked");
    assert_eq!(output.status.code(), Some(1));
    assert!(!home.join("side-effect").exists());

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn final_record_failure_keeps_attempt_fenced() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-jobs-record-{}-{}",
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
    write_job(
        &jobs.join("record-fails.1h.sh"),
        "#!/bin/sh\n\
         echo called >> \"$HOME/calls\"\n\
         rmdir \"$HOME/.local/state/plant/jobs\"\n\
         echo unavailable > \"$HOME/.local/state/plant/jobs\"\n",
    );

    let first = run(&home, &sessions, "record-fails");
    assert_eq!(first.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&first.stderr).contains("final record failed"));

    fs::remove_file(home.join(".local/state/plant/jobs")).unwrap();
    fs::create_dir_all(home.join(".local/state/plant/jobs")).unwrap();
    let second = run(&home, &sessions, "record-fails");
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("dispatch blocked"));
    assert_eq!(
        fs::read_to_string(home.join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(home
        .join(".local/state/plant/job-attempts/record-fails.json")
        .exists());

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn manual_compression_fails_with_maintenance_status_before_mutation_when_a_listener_is_unavailable()
{
    let tmp = std::env::temp_dir().join(format!(
        "plant-compress-listener-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let sessions = tmp.join("vault/sessions");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let sentinel = sessions.join("generation");
    fs::write(&sentinel, "unchanged").unwrap();

    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    let free = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let free_port = free.local_addr().unwrap().port();
    drop(free);

    let output = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["compress", "once"])
        .env("HOME", &home)
        .env("VAULT_SESSIONS", &sessions)
        .env("VAULTR_ANTHROPIC_PORT", occupied_port.to_string())
        .env("VAULTR_CODEX_PORT", free_port.to_string())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("listener ownership unavailable"));
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");

    drop(occupied);
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn second_listener_collision_releases_the_first_binding_before_any_mutation() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-compress-second-listener-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let sessions = tmp.join("vault/sessions");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let sentinel = sessions.join("generation");
    fs::write(&sentinel, "unchanged").unwrap();

    let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let first_port = first.local_addr().unwrap().port();
    drop(first);
    let second = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let second_port = second.local_addr().unwrap().port();

    let output = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["compress", "once"])
        .env("HOME", &home)
        .env("VAULT_SESSIONS", &sessions)
        .env("VAULTR_ANTHROPIC_PORT", first_port.to_string())
        .env("VAULTR_CODEX_PORT", second_port.to_string())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("listener ownership unavailable"));
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
    drop(
        TcpListener::bind(("127.0.0.1", first_port))
            .expect("failed second bind releases the already-acquired first listener"),
    );

    drop(second);
    fs::remove_dir_all(tmp).unwrap();
}
