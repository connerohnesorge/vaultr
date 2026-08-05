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
    run_args(home, sessions, &["jobs", "run", name])
}

fn run_args(home: &Path, sessions: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(args)
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
    let jobs = vault.join("jobs/shared"); // flat bucket is retired
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
fn manual_compression_command_publishes_a_script_fence() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-manual-compress-fence-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let sessions = tmp.join("vault/sessions");
    let jobs = tmp.join("vault/jobs/shared");
    let started = home.join("wrapper-started");
    let release = home.join("release-wrapper");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    write_job(
        &jobs.join("compress.30m.sh"),
        "#!/bin/sh\n\
         touch \"$HOME/wrapper-started\"\n\
         while [ ! -e \"$HOME/release-wrapper\" ]; do sleep 1; done\n",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["jobs", "run", "compress"])
        .env("HOME", &home)
        .env("VAULT_SESSIONS", &sessions)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let fence_path = home.join(".local/state/plant/job-attempts/compress.json");
    let mut observed_fence = None;
    for _ in 0..200 {
        if started.exists() && fence_path.exists() {
            observed_fence = fs::read_to_string(&fence_path).ok();
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fs::write(&release, "release\n").unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(started.exists(), "manual command must execute the wrapper");
    let fence: serde_json::Value = serde_json::from_str(
        observed_fence
            .as_deref()
            .expect("manual fence must remain visible while the wrapper runs"),
    )
    .unwrap();
    assert_eq!(fence["action"], "Script");
    assert!(
        !fence_path.exists(),
        "successful manual completion clears the fence"
    );

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn sessions_stuck_summary_is_deterministic_and_becomes_job_ledger_detail() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-watchdog-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let vault = tmp.join("vault");
    let sessions = vault.join("sessions");
    let jobs = vault.join("jobs/shared"); // flat bucket is retired
    let day = sessions.join("2026/07/20");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(home.join(".local/state/plant")).unwrap();
    fs::create_dir_all(vault.join("learnings")).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    for (sid, body) in [
        ("seal", "{}\n".repeat(6)),
        ("half-claude", "{}\n".repeat(6)),
        ("half-codex", "{}\n".repeat(6)),
        ("unlearned", "{}\n".repeat(6)),
        ("small", "{}\n".to_string()),
        ("job", "{}\n".repeat(6)),
    ] {
        let dir = day.join(sid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("turns.jsonl"), body).unwrap();
    }
    fs::write(
        vault.join("learnings/.ledger.jsonl"),
        concat!(
            "{\"session_id\":\"seal\",\"learner\":\"claude\"}\n",
            "{\"session_id\":\"seal\",\"learner\":\"codex\"}\n",
            "{\"session_id\":\"half-claude\",\"learner\":\"codex\"}\n",
            "{\"session_id\":\"half-codex\",\"learner\":\"claude\"}\n",
        ),
    )
    .unwrap();
    fs::write(home.join(".local/state/plant/job-sids.txt"), "job\n").unwrap();

    let summary = "sessions-stuck summary: seal-blocked=1 half-learned:claude=1 \
                   half-learned:codex=1 unlearned=1 sub-threshold=1 job-capture=1";
    let direct = run_args(&home, &sessions, &["sessions", "stuck", "--age", "0s"]);
    assert_eq!(direct.status.code(), Some(1));
    let stdout = String::from_utf8(direct.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.last(), Some(&summary));
    for row in [
        "seal-blocked seal",
        "half-learned:claude half-claude",
        "half-learned:codex half-codex",
        "unlearned unlearned",
        "sub-threshold small",
        "job-capture job",
    ] {
        assert!(
            lines[..lines.len() - 1]
                .iter()
                .any(|line| line.contains(row)),
            "missing detail row {row:?} in {lines:?}"
        );
    }

    write_job(
        &jobs.join("watchdog.6h.sh"),
        &format!(
            "#!/bin/sh\nexec '{}' sessions stuck --age 0s\n",
            env!("CARGO_BIN_EXE_plant")
        ),
    );
    let job = run(&home, &sessions, "watchdog");
    assert_eq!(job.status.code(), Some(1));
    let ledger = fs::read_to_string(home.join(".local/state/plant/jobs/watchdog.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
    assert_eq!(record["outcome"], "failed");
    assert_eq!(record["detail"], summary);

    fs::remove_dir_all(day.join("seal")).unwrap();
    fs::remove_dir_all(day.join("half-claude")).unwrap();
    fs::remove_dir_all(day.join("half-codex")).unwrap();
    fs::remove_dir_all(day.join("unlearned")).unwrap();
    let informational = run_args(&home, &sessions, &["sessions", "stuck", "--age", "0s"]);
    assert_eq!(informational.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(informational.stdout)
            .unwrap()
            .lines()
            .last(),
        Some(
            "sessions-stuck summary: seal-blocked=0 half-learned:claude=0 \
             half-learned:codex=0 unlearned=0 sub-threshold=1 job-capture=1"
        )
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
    let jobs = vault.join("jobs/shared"); // flat bucket is retired
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
    let jobs = vault.join("jobs/shared"); // flat bucket is retired
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
