//! One state root for every subsystem. Two resolvers used to disagree:
//! `ca::state_dir` honored `XDG_STATE_HOME` while `state::dir` read only
//! `HOME`, so an operator who set `XDG_STATE_HOME` got a CA in one directory
//! and job ledgers, attempt fences, and capture staging in another. The
//! "one place to wipe" contract was false, and `plant jobs unblock` reported
//! "no attempt fence" for a fence sitting in plain sight under the state root
//! the operator had chosen.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A nonretryable fence with no receipt and no ledger record: the shape of the
/// incident `plant jobs unblock` exists to resolve.
fn abandoned_fence(state: &Path, job: &str, attempt: &str) {
    let dir = state.join("job-attempts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{job}.json")),
        format!(r#"{{"id":"{attempt}","started_ts":1753372980,"retryable":false}}"#),
    )
    .unwrap();
}

fn unique_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "plant-state-root-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn xdg_state_home_moves_the_job_ledger_with_the_rest_of_the_state_root() {
    let root = unique_root("xdg");
    // A HOME that is real but deliberately not where the state belongs: if the
    // job ledger still resolves through HOME, the fence below stays untouched.
    let decoy_home = root.join("decoy-home");
    let state = root.join("xdg").join("plant");
    std::fs::create_dir_all(&decoy_home).unwrap();
    abandoned_fence(&state, "probe-job", "abandoned-attempt-0001");

    let output = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["jobs", "unblock", "probe-job"])
        .env("HOME", &decoy_home)
        .env("XDG_STATE_HOME", root.join("xdg"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        stdout.contains("cleared the fence"),
        "job ledger did not follow XDG_STATE_HOME: {stdout}"
    );
    assert!(
        !state.join("job-attempts/probe-job.json").exists(),
        "fence survived the unblock"
    );
    let ledger = std::fs::read_to_string(state.join("jobs/probe-job.jsonl")).unwrap();
    assert_eq!(ledger.lines().count(), 1, "exactly one record: {ledger}");
    assert!(ledger.contains(r#""outcome":"failed""#), "{ledger}");
    assert!(ledger.contains("abandoned-attempt-0001"), "{ledger}");
    assert!(
        !decoy_home.join(".local/state/plant").exists(),
        "state leaked into HOME while XDG_STATE_HOME was set"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn home_still_resolves_the_state_root_when_xdg_state_home_is_unset() {
    let root = unique_root("home");
    let state = root.join(".local/state/plant");
    abandoned_fence(&state, "probe-job", "abandoned-attempt-0002");

    let output = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["jobs", "unblock", "probe-job"])
        .env("HOME", &root)
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains("cleared the fence"), "{stdout}");
    let ledger = std::fs::read_to_string(state.join("jobs/probe-job.jsonl")).unwrap();
    assert!(ledger.contains("abandoned-attempt-0002"), "{ledger}");

    std::fs::remove_dir_all(root).unwrap();
}

/// An empty `XDG_STATE_HOME` is not a choice of directory. Treating it as one
/// would put the state root at `/plant`.
#[test]
fn an_empty_xdg_state_home_falls_back_to_home() {
    let root = unique_root("empty");
    let state = root.join(".local/state/plant");
    abandoned_fence(&state, "probe-job", "abandoned-attempt-0003");

    let output = Command::new(env!("CARGO_BIN_EXE_plant"))
        .args(["jobs", "unblock", "probe-job"])
        .env("HOME", &root)
        .env("XDG_STATE_HOME", "")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains("cleared the fence"), "{stdout}");
    let ledger = std::fs::read_to_string(state.join("jobs/probe-job.jsonl")).unwrap();
    assert!(ledger.contains("abandoned-attempt-0003"), "{ledger}");

    std::fs::remove_dir_all(root).unwrap();
}
