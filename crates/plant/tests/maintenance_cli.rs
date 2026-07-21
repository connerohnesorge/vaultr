use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn run(home: &Path, sessions: &Path, path: &Path, args: &[&str]) -> Output {
    let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let second = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let first_port = first.local_addr().unwrap().port();
    let second_port = second.local_addr().unwrap().port();
    drop((first, second));
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(args)
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .env("VAULTR_ANTHROPIC_PORT", first_port.to_string())
        .env("VAULTR_CODEX_PORT", second_port.to_string())
        .env("PATH", path)
        .output()
        .unwrap()
}

fn assert_inventory_failure(output: Output, command: &str, path: &Path) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "{command} emitted partial output");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(command) && stderr.contains(&path.display().to_string()),
        "{command} did not identify the failed inventory path: {stderr}"
    );
}

#[test]
fn maintenance_commands_propagate_inventory_failures_and_preserve_no_work() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-maintenance-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let bin = tmp.join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let zstd = bin.join("zstd");
    fs::write(&zstd, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&zstd, fs::Permissions::from_mode(0o755)).unwrap();

    let missing = tmp.join("missing");
    for (args, command) in [
        (&["sessions", "eligible"][..], "sessions eligible"),
        (&["sessions", "stuck"][..], "sessions stuck"),
        (&["compress", "once"][..], "compress once"),
    ] {
        assert_inventory_failure(run(&home, &missing, &bin, args), command, &missing);
    }

    let empty = tmp.join("empty");
    fs::create_dir(&empty).unwrap();
    for (args, expected) in [
        (&["sessions", "eligible"][..], 1),
        (&["sessions", "stuck"][..], 0),
        (&["compress", "once"][..], 0),
    ] {
        let output = run(&home, &empty, &bin, args);
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let unreadable = tmp.join("unreadable");
    let day = unreadable.join("2026/07/20");
    fs::create_dir_all(&day).unwrap();
    fs::set_permissions(&day, fs::Permissions::from_mode(0o000)).unwrap();
    for (args, command) in [
        (&["sessions", "eligible"][..], "sessions eligible"),
        (&["sessions", "stuck"][..], "sessions stuck"),
        (&["compress", "once"][..], "compress once"),
    ] {
        assert_inventory_failure(run(&home, &unreadable, &bin, args), command, &day);
    }
    fs::set_permissions(&day, fs::Permissions::from_mode(0o700)).unwrap();

    let session = tmp.join("unreadable-generation/2026/07/20/session");
    fs::create_dir_all(&session).unwrap();
    for name in ["turns.jsonl", "turns.jsonl.zst"] {
        let generation = session.join(name);
        fs::write(&generation, "{}\n".repeat(6)).unwrap();
        fs::set_permissions(&generation, fs::Permissions::from_mode(0o000)).unwrap();
        for (args, command) in [
            (&["sessions", "eligible"][..], "sessions eligible"),
            (&["sessions", "stuck"][..], "sessions stuck"),
            (&["compress", "once"][..], "compress once"),
        ] {
            assert_inventory_failure(
                run(&home, &tmp.join("unreadable-generation"), &bin, args),
                command,
                &generation,
            );
        }
        fs::set_permissions(&generation, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(generation).unwrap();
    }

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn manual_compression_recovers_owned_capture_state_before_sweeping() {
    let tmp = std::env::temp_dir().join(format!(
        "plant-manual-recovery-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = tmp.join("home");
    let bin = tmp.join("bin");
    let sessions = tmp.join("vault/sessions");
    let sid = "00000000-0000-4000-8000-000000000048";
    let dir = sessions.join("2026/07/20").join(sid);
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&dir).unwrap();
    let zstd = bin.join("zstd");
    fs::write(&zstd, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&zstd, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.join("turns.jsonl"), b"").unwrap();
    let root = fs::canonicalize(&sessions).unwrap().display().to_string();
    let request = serde_json::json!({
        "schema_version": 1,
        "request_id": "00000000-0000-4000-8000-000000000049",
        "session_id": sid,
        "request": {"body_delta": {"history": {"key": "messages", "prefix_length": 0, "append": []}}}
    });
    fs::write(
        dir.join("state.json"),
        serde_json::json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": sid,
            "request_body": {},
            "capture_order": {
                "next_sequence": 1,
                "next_to_drain": 0,
                "pending": {"0": request},
                "root": root
            }
        })
        .to_string(),
    )
    .unwrap();

    let output = run(
        &home,
        &sessions,
        &bin,
        &["compress", "once", "--idle", "0s"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_str(fs::read_to_string(dir.join("turns.jsonl")).unwrap().trim()).unwrap();
    assert_eq!(envelope["response"]["complete"], false);
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["capture_order"]["next_to_drain"], 1);

    fs::remove_dir_all(tmp).unwrap();
}
