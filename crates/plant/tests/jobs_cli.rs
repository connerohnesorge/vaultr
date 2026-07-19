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
