use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn run(home: &Path, sessions: &Path, path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(args)
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .env("PATH", path)
        .output()
        .unwrap()
}

#[test]
fn maintenance_commands_propagate_traversal_failures_and_preserve_no_work() {
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
    for args in [
        &["sessions", "eligible"][..],
        &["sessions", "stuck"][..],
        &["compress", "once"][..],
    ] {
        let output = run(&home, &missing, &bin, args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
    for args in [
        &["sessions", "eligible"][..],
        &["sessions", "stuck"][..],
        &["compress", "once"][..],
    ] {
        let output = run(&home, &unreadable, &bin, args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::set_permissions(&day, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(tmp).unwrap();
}
