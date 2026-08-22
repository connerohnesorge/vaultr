use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct TestVault {
    _temporary: tempfile::TempDir,
    vault: PathBuf,
    sessions: PathBuf,
    state: PathBuf,
}

impl TestVault {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let vault = temporary.path().join("vault");
        let sessions = vault.join("sessions");
        let state = temporary.path().join("state");
        fs::create_dir_all(sessions.join(".meta")).unwrap();
        Self {
            _temporary: temporary,
            vault,
            sessions,
            state,
        }
    }

    fn add_session(&self, id: &str, capture: &str) -> PathBuf {
        let session = self.sessions.join("2026/01/01").join(id);
        fs::create_dir_all(&session).unwrap();
        fs::write(
            self.sessions.join(".meta").join(format!("{id}.json")),
            r#"{"original_start":"2026-01-01T00:00:00Z","last_observation":"2026-01-01T01:00:00Z","harness":"claude-code","cwd":"/work","git_branch":"main","model":"test"}"#,
        )
        .unwrap();
        let path = session.join("turns.jsonl");
        fs::write(&path, capture).unwrap();
        path
    }

    fn index(&self, rebuild: bool) -> Output {
        let mut command = self.command();
        command.args(["session", "index"]);
        command.arg(if rebuild { "--rebuild" } else { "--update" });
        command.args(["--workers", "2"]);
        let output = command.output().unwrap();
        assert_success(&output);
        output
    }

    fn search(&self, query: &str, extra: &[&str]) -> Value {
        let mut command = self.command();
        command.args(["session", "search", query, "--json"]);
        command.args(extra);
        let output = command.output().unwrap();
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vaultr"));
        command
            .env("XDG_STATE_HOME", &self.state)
            .args(["--vault", self.sessions.to_str().unwrap()]);
        command
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn session_index_help_exposes_the_scheduled_job_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_vaultr"))
        .args(["session", "index", "--help"])
        .output()
        .unwrap();
    assert_success(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--update"), "{help}");
    assert!(help.contains("--workers"), "{help}");
}

fn prompt_capture(text: &str) -> String {
    format!(
        "{}\n",
        json!({
            "harness": "claude-code",
            "observed_at": "2026-01-01T00:00:00Z",
            "request": {"body_delta": {"history": {
                "key": "messages",
                "prefix_length": 0,
                "append": [{"role": "user", "content": text}]
            }}}
        })
    )
}

#[test]
fn incremental_update_replaces_only_changed_sessions() {
    let vault = TestVault::new();
    let changed_path = vault.add_session(
        "11111111-1111-1111-1111-111111111111",
        &prompt_capture("old unique needle"),
    );
    vault.add_session(
        "22222222-2222-2222-2222-222222222222",
        &prompt_capture("stable unique needle"),
    );

    let first = vault.index(false);
    assert!(String::from_utf8_lossy(&first.stdout).contains("(2 changed)"));
    assert!(vault.state.join("vaultr/sessions").is_dir());
    assert!(!vault.vault.join(".local/state/vaultr").exists());
    let unchanged = vault.index(false);
    assert!(String::from_utf8_lossy(&unchanged.stdout).contains("(0 changed)"));

    fs::write(&changed_path, prompt_capture("new unique needle")).unwrap();
    let updated = vault.index(false);
    assert!(String::from_utf8_lossy(&updated.stdout).contains("(1 changed)"));
    assert_eq!(vault.search("old unique", &[])["total"], 0);
    assert_eq!(vault.search("new unique", &[])["total"], 1);
    assert_eq!(vault.search("stable unique", &[])["total"], 1);

    let sealed = changed_path.with_extension("jsonl.zst");
    let encoded = zstd::encode_all(fs::File::open(&changed_path).unwrap(), 3).unwrap();
    fs::write(&sealed, encoded).unwrap();
    fs::remove_file(&changed_path).unwrap();
    let sealed_update = vault.index(false);
    assert!(String::from_utf8_lossy(&sealed_update.stdout).contains("(1 changed)"));
    assert_eq!(vault.search("new unique", &[])["total"], 1);
}

#[test]
fn repeated_occurrences_collapse_without_losing_members() {
    let vault = TestVault::new();
    let first = json!({
        "harness": "claude-code",
        "request": {"body_delta": {"history": {
            "key": "messages",
            "prefix_length": 0,
            "append": [{"role": "user", "content": "repeat needle"}]
        }}}
    });
    let second = json!({
        "harness": "claude-code",
        "request": {"body_delta": {"history": {
            "key": "messages",
            "prefix_length": 1,
            "append": [{"role": "user", "content": "repeat needle"}]
        }}}
    });
    vault.add_session(
        "33333333-3333-3333-3333-333333333333",
        &format!("{first}\n{second}\n"),
    );
    vault.index(false);

    let collapsed = vault.search("repeat needle", &[]);
    assert_eq!(collapsed["total"], 2);
    assert_eq!(collapsed["shown"], 1);
    assert_eq!(collapsed["hits"][0]["duplicates"], 2);
    assert_eq!(
        collapsed["hits"][0]["duplicate_members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(collapsed["hits"][0].get("body").is_none());
    assert!(collapsed["hits"][0]["snippet"].is_string());
    assert_ne!(collapsed["hits"][0]["snippet"], "repeat needle");

    let expanded = vault.search("repeat needle", &["--no-collapse"]);
    assert_eq!(expanded["shown"], 2);
}

#[test]
fn compacted_completed_output_is_searchable_but_not_final() {
    let vault = TestVault::new();
    let first = json!({
        "harness": "claude-code",
        "observed_at": "2026-01-01T00:00:00Z",
        "request": {"body_delta": {"history": {
            "key": "messages",
            "prefix_length": 0,
            "append": [{"role": "user", "content": "first prompt"}]
        }}},
        "response": {
            "complete": true,
            "events": [
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "text", "text": ""}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta", "text": "vanished assistant needle"}},
                {"type": "message_stop"}
            ]
        }
    });
    let second = json!({
        "harness": "claude-code",
        "observed_at": "2026-01-01T00:01:00Z",
        "request": {"body_delta": {"history": {
            "key": "messages",
            "prefix_length": 0,
            "append": [{"role": "user", "content": "current prompt"}]
        }}}
    });
    vault.add_session(
        "44444444-4444-4444-4444-444444444444",
        &format!("{first}\n{second}\n"),
    );
    vault.index(false);

    let all = vault.search("vanished assistant", &[]);
    assert_eq!(all["total"], 1);
    assert_eq!(all["hits"][0]["compacted"], true);
    assert_eq!(all["hits"][0]["timestamp"], "2026-01-01T00:00:00Z");
    assert_eq!(
        vault.search("vanished assistant", &["--final-only"])["total"],
        0
    );
}

#[test]
fn curated_boundary_orphan_prompts_and_literal_fragments_work_end_to_end() {
    let vault = TestVault::new();
    fs::create_dir_all(vault.vault.join("learnings")).unwrap();
    fs::create_dir_all(vault.vault.join("preferences")).unwrap();
    fs::create_dir_all(vault.vault.join("input/2026/01/01")).unwrap();
    fs::write(
        vault.vault.join("learnings/search.md"),
        "curated learning needle",
    )
    .unwrap();
    fs::write(
        vault.vault.join("preferences/private.md"),
        "excluded preference needle",
    )
    .unwrap();
    let captured_id = "55555555-5555-5555-5555-555555555555";
    let capture = format!(
        "{}\n",
        json!({
            "harness": "claude-code",
            "request": {"body_delta": {"history": {
                "key": "messages",
                "prefix_length": 0,
                "append": [
                    {"role": "user", "content": "inspect the source"},
                    {"role": "assistant", "content": [{
                        "type": "tool_use",
                        "name": "Read",
                        "input": {"path": "/work/SpecificIdentifier.rs"}
                    }]}
                ]
            }}}
        })
    );
    vault.add_session(captured_id, &capture);
    fs::write(
        vault.vault.join("input/2026/01/01/prompts.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"session": format!("sessions/2026/01/01/{captured_id}"), "prompt": "captured sidecar needle"}),
            json!({"session": "sessions/2026/01/01/orphan", "prompt": "orphan sidecar needle"}),
        ),
    )
    .unwrap();
    vault.index(false);

    let curated = vault.search("curated learning", &["--curated"]);
    assert_eq!(curated["curated_total"], 1);
    assert_eq!(curated["curated_hits"][0]["path"], "learnings/search.md");
    assert_eq!(
        vault.search("excluded preference", &["--curated"])["curated_total"],
        0
    );
    assert_eq!(
        vault.search("orphan sidecar", &["--curated"])["curated_total"],
        1
    );
    assert_eq!(
        vault.search("captured sidecar", &["--curated"])["curated_total"],
        0
    );
    assert_eq!(vault.search("cific", &[])["total"], 1);
}

#[test]
fn search_requires_a_ready_index() {
    let vault = TestVault::new();
    let output = vault
        .command()
        .args(["session", "search", "needle"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("vaultr session index --update"));
}

#[test]
fn index_requires_an_explicit_lifecycle_mode() {
    let vault = TestVault::new();
    vault.add_session(
        "66666666-6666-6666-6666-666666666666",
        &prompt_capture("needle"),
    );
    let output = vault.command().args(["session", "index"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--update"));
}
