//! Operator-visible surfaces for dropped-turn accounting: the coverage audit
//! marks a capture known-incomplete, and the health job alerts on both classes.

use std::path::Path;
use std::process::Command;

fn session(root: &Path, sid: &str, dropped: u64) {
    let dir = root.join("2026/07/17").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("turns.jsonl"),
        r#"{"harness":"claude-code","observed_at":"2026-07-17T19:00:00.000Z","response":{"headers":{"request-id":"req_A"}}}"#.to_string() + "\n",
    )
    .unwrap();
    let transcript = root.join(format!("{sid}.transcript.jsonl"));
    std::fs::write(
        &transcript,
        "{\"type\":\"assistant\",\"requestId\":\"req_A\",\"timestamp\":\"2026-07-17T19:00:00.000Z\"}\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".meta")).unwrap();
    std::fs::write(
        root.join(".meta").join(format!("{sid}.json")),
        format!(
            r#"{{"session_id":"{sid}","original_start":"2026-07-17T19:00:00.000Z","session_start_source":"wire","transcript_path":"{}","dropped_turns":{dropped}}}"#,
            transcript.display()
        ),
    )
    .unwrap();
}

#[test]
fn recorded_drops_reach_the_coverage_audit_and_the_health_job() {
    let root = std::env::temp_dir().join(format!("plant-drops-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lossy = "00000000-0000-4000-8000-000000000041";
    let clean = "00000000-0000-4000-8000-000000000042";
    session(&root, lossy, 7);
    session(&root, clean, 0);

    let coverage = |sid: &str| {
        Command::new(env!("CARGO_BIN_EXE_plant"))
            .args(["sessions", "coverage", sid])
            .env("VAULT_SESSIONS", &root)
            .output()
            .unwrap()
    };
    let stdout = String::from_utf8(coverage(lossy).stdout).unwrap();
    assert!(
        stdout.contains("KNOWN-INCOMPLETE: 7 recorded dropped turn(s)"),
        "{stdout}"
    );
    let stdout = String::from_utf8(coverage(clean).stdout).unwrap();
    assert!(!stdout.contains("KNOWN-INCOMPLETE"), "{stdout}");

    // A floor above any real volume is the full-volume simulation for the alert.
    let stuck = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["sessions", "stuck"])
        .env("VAULT_SESSIONS", &root)
        .env("PLANT_CAPTURE_HEADROOM_BYTES", u64::MAX.to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8(stuck.stdout).unwrap();
    assert!(stdout.contains("low-headroom alert:"), "{stdout}");
    assert!(
        stdout.contains(&format!("dropped-turn alert: {lossy} dropped=7")),
        "{stdout}"
    );
    assert!(!stdout.contains(clean), "{stdout}");

    std::fs::remove_dir_all(root).unwrap();
}
