use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "plant-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn plant_output(
    home: &Path,
    sessions: &Path,
    port: u16,
    args: &[&str],
    input: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_plant"));
    command
        .args(args)
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .env("VAULTR_ANTHROPIC_PORT", port.to_string())
        .env("VAULTR_CODEX_PORT", port.to_string());
    if let Some(input) = input {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        command.output().unwrap()
    }
}

#[test]
fn cli_grammar_subprocess_table_keeps_daemon_bare_and_rejects_every_other_miss() {
    let root = temp("cli-contract");
    let home = root.join("home");
    let sessions = root.join("vault/sessions");
    fs::create_dir_all(home.join(".dotfiles")).unwrap();
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    write_executable(
        &home.join(".local/bin/herdr"),
        "#!/bin/sh\necho called >> \"$HOME/herdr-called\"\nexit 1\n",
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    struct Case<'a> {
        name: &'a str,
        args: &'a [&'a str],
        input: Option<&'a str>,
        code: i32,
    }
    let cases = [
        Case {
            name: "help",
            args: &["--help"],
            input: None,
            code: 2,
        },
        Case {
            name: "typo",
            args: &["sesions", "stuck"],
            input: None,
            code: 2,
        },
        Case {
            name: "incomplete grammar",
            args: &["sessions"],
            input: None,
            code: 2,
        },
        Case {
            name: "invalid cli enum",
            args: &["agent", "run", "--cli", "other"],
            input: Some("do nothing"),
            code: 2,
        },
        Case {
            name: "invalid cleanup enum",
            args: &["agent", "run", "--cli", "claude", "--cleanup", "sometimes"],
            input: Some("do nothing"),
            code: 2,
        },
        Case {
            name: "self-test with extra argv",
            args: &["--self-test", "extra"],
            input: None,
            code: 2,
        },
        Case {
            name: "valid claude",
            args: &["agent", "run", "--cli", "claude", "--cleanup", "never"],
            input: Some("do nothing"),
            code: 75,
        },
        Case {
            name: "valid codex",
            args: &["agent", "run", "--cli", "codex", "--cleanup", "on-success"],
            input: Some("do nothing"),
            code: 75,
        },
        Case {
            name: "bare daemon",
            args: &[],
            input: None,
            code: 0,
        },
    ];

    for case in cases {
        let output = plant_output(&home, &sessions, port, case.args, case.input);
        assert_eq!(
            output.status.code(),
            Some(case.code),
            "{}: stdout={} stderr={}",
            case.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if case.code == 2 {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("usage: plant"),
                "{} did not report usage",
                case.name
            );
            assert!(!home.join("herdr-called").exists());
            assert!(!home.join(".local/state/plant/job-sids.txt").exists());
        }
        if case.name == "bare daemon" {
            assert!(String::from_utf8_lossy(&output.stdout).contains("another instance owns it"));
        }
    }

    fs::remove_dir_all(root).unwrap();
}

fn create_session(sessions: &Path, sid: &str) {
    let dir = sessions.join("2026/07/20").join(sid);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
}

fn claim(home: &Path, sessions: &Path, learner: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args([
            "sessions",
            "eligible",
            "--learner",
            learner,
            "--idle",
            "0s",
            "--claim",
            "1h",
        ])
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .output()
        .unwrap()
}

fn barrier_claim(home: &Path, sessions: &Path, ready: &Path, gate: &Path) -> Child {
    Command::new("/bin/sh")
        .args([
            "-c",
            "touch \"$READY\"; while [ ! -e \"$GATE\" ]; do sleep 0.01; done; \
             exec \"$PLANT\" sessions eligible --learner claude --idle 0s --claim 1h",
        ])
        .env("READY", ready)
        .env("GATE", gate)
        .env("PLANT", env!("CARGO_BIN_EXE_plant"))
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn learner_claim_is_cross_process_atomic_independent_and_recovers_after_expiry() {
    let root = temp("learner-claim");
    let home = root.join("home");
    let sessions = root.join("vault/sessions");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(root.join("vault/learnings")).unwrap();
    create_session(&sessions, "claimable");

    let gate = root.join("gate");
    let ready_a = root.join("ready-a");
    let ready_b = root.join("ready-b");
    let first = barrier_claim(&home, &sessions, &ready_a, &gate);
    let second = barrier_claim(&home, &sessions, &ready_b, &gate);
    for _ in 0..500 {
        if ready_a.exists() && ready_b.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ready_a.exists() && ready_b.exists(),
        "subprocess barrier was not reached"
    );
    fs::write(&gate, "").unwrap();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.code() == Some(0) && !output.stdout.is_empty())
            .count(),
        1,
        "exactly one same-learner process must receive the batch"
    );
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.code() == Some(1) && output.stdout.is_empty())
            .count(),
        1
    );

    let claude_lease = root.join("vault/learnings/.inflight-claude.json");
    let published = fs::read_to_string(&claude_lease).unwrap();
    let lease: serde_json::Value = serde_json::from_str(&published).unwrap();
    assert_eq!(lease["sids"], serde_json::json!(["claimable"]));

    let blocked = claim(&home, &sessions, "claude");
    assert_eq!(blocked.status.code(), Some(1));
    assert!(blocked.stdout.is_empty());
    assert_eq!(fs::read_to_string(&claude_lease).unwrap(), published);

    let codex = claim(&home, &sessions, "codex");
    assert_eq!(codex.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&codex.stdout).contains("claimable"));
    assert_eq!(fs::read_to_string(&claude_lease).unwrap(), published);
    assert!(root.join("vault/learnings/.inflight-codex.json").is_file());

    fs::write(
        &claude_lease,
        serde_json::json!({"sids": ["claimable"], "expires_at": 0}).to_string(),
    )
    .unwrap();
    let recovered = claim(&home, &sessions, "claude");
    assert_eq!(recovered.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&recovered.stdout).contains("claimable"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn learner_claim_publication_failure_emits_no_paths() {
    let root = temp("learner-claim-unwritable");
    let home = root.join("home");
    let sessions = root.join("vault/sessions");
    let learnings = root.join("vault/learnings");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&learnings).unwrap();
    create_session(&sessions, "must-not-escape");
    fs::write(learnings.join(".inflight-claude.json.lock"), "").unwrap();
    fs::set_permissions(&learnings, fs::Permissions::from_mode(0o555)).unwrap();

    let output = claim(&home, &sessions, "claude");
    fs::set_permissions(&learnings, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "unclaimed paths escaped on stdout"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("claim failed"));
    assert!(!learnings.join(".inflight-claude.json").exists());

    fs::remove_dir_all(root).unwrap();
}
