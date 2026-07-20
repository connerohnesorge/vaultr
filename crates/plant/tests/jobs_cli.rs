use std::fs;
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

    // not executable at all: spawn fails and is recorded
    fs::write(jobs.join("nonexec.1h.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let output = run(&home, &sessions, "nonexec");
    assert_eq!(output.status.code(), Some(1));

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
    assert!(fs::read_to_string(ledger.join("tsjob.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"success\""));
    assert!(fs::read_to_string(ledger.join("noshebang.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"success\""));
    assert!(fs::read_to_string(ledger.join("nonexec.jsonl"))
        .unwrap()
        .contains("\"detail\":\"spawn:"));

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
    let jobs = vault.join("jobs");
    let day = sessions.join("2026/07/20");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(home.join(".local/state/plant")).unwrap();
    fs::create_dir_all(vault.join("learnings")).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    for (sid, body) in [
        ("seal", "{}\n".repeat(6)),
        ("half-codex", "{}\n".repeat(6)),
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
            "{\"session_id\":\"half-codex\",\"learner\":\"claude\"}\n",
        ),
    )
    .unwrap();
    fs::write(home.join(".local/state/plant/job-sids.txt"), "job\n").unwrap();

    let summary = "sessions-stuck summary: seal-blocked=1 half-learned:claude=0 \
                   half-learned:codex=1 unlearned=0 sub-threshold=1 job-capture=1";
    let direct = run_args(&home, &sessions, &["sessions", "stuck", "--age", "0s"]);
    assert_eq!(direct.status.code(), Some(1));
    let stdout = String::from_utf8(direct.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.last(), Some(&summary));
    for row in [
        "seal-blocked seal",
        "half-learned:codex half-codex",
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
    fs::remove_dir_all(day.join("half-codex")).unwrap();
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
