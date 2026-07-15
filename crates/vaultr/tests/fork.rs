//! Fork writer tests. All writes go to temp dirs — never the real ~/.claude,
//! ~/.codex, or the vault. Integration smokes read the real vault read-only
//! and skip when it is absent.

use chrono::{Local, TimeZone};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use vaultr::fork::{self, ForkOptions, Target};
use vaultr::{claude_writer, codex_writer};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn sample_messages() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "hello <system-reminder>secret scaffolding</system-reminder> world",
             "cache_control": {"type": "ephemeral"}}
        ]}),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "hi"},
            {"type": "tool_use", "id": "toolu_abc", "name": "Bash", "input": {"command": "ls"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_abc", "content": "a.txt"}
        ]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
    ]
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ---------- Claude writer ----------

#[test]
fn claude_encoded_dir_and_chain() {
    let dir = tmp();
    let (id, path) = claude_writer::write(
        dir.path(),
        "/Users/x/.dotfiles",
        Some("main"),
        Some("claude-opus-4-8"),
        &sample_messages(),
    )
    .unwrap();
    assert!(path
        .to_string_lossy()
        .contains("projects/-Users-x--dotfiles/"));
    assert_eq!(path.file_stem().unwrap().to_str().unwrap(), id);
    let u = uuid::Uuid::parse_str(&id).unwrap();
    assert_eq!(u.get_version_num(), 4);

    let recs = read_jsonl(&path);
    // parentUuid chain over the conversational records.
    let conv: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "user" || r["type"] == "assistant")
        .collect();
    assert_eq!(conv.len(), 4);
    assert!(conv[0]["parentUuid"].is_null());
    for w in conv.windows(2) {
        assert_eq!(w[1]["parentUuid"], w[0]["uuid"]);
    }
    for r in &conv {
        assert_eq!(r["sessionId"].as_str().unwrap(), id);
        assert_eq!(r["version"], "2.1.210");
        assert_eq!(r["userType"], "external");
        assert_eq!(r["gitBranch"], "main");
    }
    // Trailing last-prompt points at the leaf and carries visible text only.
    let last = recs.last().unwrap();
    assert_eq!(last["type"], "last-prompt");
    assert_eq!(last["leafUuid"], conv.last().unwrap()["uuid"]);
    let lp = last["lastPrompt"].as_str().unwrap();
    assert!(lp.contains("hello"));
    assert!(!lp.contains("secret scaffolding"));
}

#[test]
fn claude_mode_0600() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        let (_, path) =
            claude_writer::write(dir.path(), "/", None, None, &sample_messages()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn claude_collision_refused_nothing_clobbered() {
    let dir = tmp();
    let id = "11111111-1111-4111-8111-111111111111";
    let dest =
        claude_writer::write_with_id(dir.path(), "/", None, None, &sample_messages(), id).unwrap();
    let before = std::fs::read_to_string(&dest).unwrap();
    let err = claude_writer::write_with_id(dir.path(), "/", None, None, &sample_messages(), id)
        .unwrap_err();
    assert!(err.to_string().contains("refusing to overwrite"));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), before);
    // No temp litter.
    let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn claude_empty_history_writes_nothing() {
    let dir = tmp();
    assert!(claude_writer::write(dir.path(), "/", None, None, &[]).is_err());
    assert!(
        !dir.path().join("projects").exists() || {
            // even if the projects dir was made, no session file exists
            walk_files(dir.path()).is_empty()
        }
    );
}

#[test]
fn claude_passthrough_strips_cache_control_only() {
    let dir = tmp();
    let msgs = sample_messages();
    let (_, path) = claude_writer::write(dir.path(), "/", None, None, &msgs).unwrap();
    let recs = read_jsonl(&path);
    let conv: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "user" || r["type"] == "assistant")
        .collect();
    let expected: Vec<Value> = msgs
        .iter()
        .map(|m| claude_writer::strip_cache_control(m.clone()))
        .collect();
    for (rec, exp) in conv.iter().zip(&expected) {
        assert_eq!(rec["message"]["content"], exp["content"]);
        assert_eq!(rec["message"]["role"], exp["role"]);
    }
    // system-reminder text preserved byte-for-byte in the record.
    let first = conv[0]["message"]["content"][0]["text"].as_str().unwrap();
    assert!(first.contains("<system-reminder>secret scaffolding</system-reminder>"));
    // cache_control gone.
    assert!(conv[0]["message"]["content"][0]
        .get("cache_control")
        .is_none());
}

// ---------- Codex writer ----------

fn codex_items() -> Vec<Value> {
    vec![
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "<permissions instructions>stuff"}]}),
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /x"}]}),
        json!({"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "do the thing"}]}),
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "OPAQUE=="}),
        json!({"type": "function_call", "id": "fc_1", "name": "shell",
               "arguments": "{\"command\":[\"bash\",\"-lc\",\"ls\"]}", "call_id": "call_1"}),
        json!({"type": "function_call_output", "call_id": "call_1",
               "output": [{"type": "input_text", "text": "a.txt"}]}),
        json!({"type": "message", "role": "assistant",
               "content": [{"type": "output_text", "text": "done"}]}),
    ]
}

#[test]
fn codex_filename_local_time_and_uuidv7() {
    let home = tmp();
    let start = Local.with_ymd_and_hms(2026, 7, 3, 23, 58, 7).unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let path = codex_writer::write_with_id(
        home.path(),
        "/tmp",
        Some("main"),
        &codex_items(),
        Some("base"),
        &id,
        start,
    )
    .unwrap();
    let rel = path.strip_prefix(home.path()).unwrap();
    assert_eq!(
        rel,
        PathBuf::from(format!(
            "sessions/2026/07/03/rollout-2026-07-03T23-58-07-{id}.jsonl"
        ))
    );
    let recs = read_jsonl(&path);
    let meta = &recs[0];
    assert_eq!(meta["type"], "session_meta");
    assert_eq!(meta["payload"]["session_id"].as_str().unwrap(), id);
    assert_eq!(meta["payload"]["id"].as_str().unwrap(), id);
    assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 7);
    assert_eq!(meta["payload"]["cli_version"], "0.144.4");
    assert_eq!(meta["payload"]["base_instructions"]["text"], "base");
    assert_eq!(meta["payload"]["git"]["branch"], "main");
    // session_meta timestamp is the UTC rendering of the local start.
    assert_eq!(
        meta["payload"]["timestamp"],
        start
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
}

#[test]
fn codex_passthrough_fidelity_and_preview_events() {
    let home = tmp();
    let items = codex_items();
    let (id, path) = codex_writer::write(home.path(), "/tmp", None, &items, None).unwrap();
    assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 7);
    let recs = read_jsonl(&path);
    assert_eq!(recs[0]["type"], "session_meta");
    let written: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "response_item")
        .map(|r| &r["payload"])
        .collect();
    assert_eq!(written.len(), items.len());
    for (w, orig) in written.iter().zip(&items) {
        assert_eq!(*w, orig, "response_item must round-trip verbatim");
    }
    // exactly one user_message preview (the typed one, not AGENTS.md scaffolding)
    let previews: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "event_msg" && r["payload"]["type"] == "user_message")
        .collect();
    assert_eq!(previews.len(), 1);
    assert_eq!(previews[0]["payload"]["message"], "do the thing");
    // 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn codex_collision_refused() {
    let home = tmp();
    let start = Local.with_ymd_and_hms(2026, 1, 1, 1, 1, 1).unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let items = codex_items();
    codex_writer::write_with_id(home.path(), "/", None, &items, None, &id, start).unwrap();
    let err =
        codex_writer::write_with_id(home.path(), "/", None, &items, None, &id, start).unwrap_err();
    assert!(err.to_string().contains("refusing to overwrite"));
}

#[test]
fn codex_passthrough_prep_drops_scaffolding() {
    let mut msgs = vec![
        json!({"type": "additional_tools", "role": "developer"}),
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "You are Codex, an agent based on GPT-5. ..."}]}),
    ];
    msgs.extend(codex_items());
    let (items, base) = fork::prepare_codex_passthrough(&msgs);
    assert_eq!(
        base.as_deref(),
        Some("You are Codex, an agent based on GPT-5. ...")
    );
    assert_eq!(items, codex_items(), "later developer messages are kept");
}

// ---------- fork() end-to-end over a fixture vault ----------

fn fixture_vault(
    harness: &str,
    history: &[Value],
    cwd: Option<&str>,
) -> (tempfile::TempDir, String) {
    let root = tmp();
    let id = if harness == "codex" {
        uuid::Uuid::now_v7().to_string()
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    let meta_dir = root.path().join(".meta");
    std::fs::create_dir_all(&meta_dir).unwrap();
    std::fs::write(
        meta_dir.join(format!("{id}.json")),
        serde_json::to_string(&json!({
            "schema_version": 1,
            "harness": harness,
            "session_id": id,
            "cwd": cwd,
            "git_branch": "main",
            "model": if harness == "codex" { "gpt-5.6-sol" } else { "claude-opus-4-8" },
            "original_start": "2026-07-01T00:00:00.000Z",
            "last_observation": "2026-07-01T00:00:00.000Z",
        }))
        .unwrap(),
    )
    .unwrap();
    let sdir = root.path().join("2026/07/01").join(&id);
    std::fs::create_dir_all(&sdir).unwrap();
    let key = if harness == "codex" {
        "input"
    } else {
        "messages"
    };
    let envelope = json!({
        "schema_version": 1,
        "harness": harness,
        "session_id": id,
        "request": {"body_delta": {"history": {"key": key, "prefix_length": 0, "append": history}}},
        "response": {"complete": false}
    });
    std::fs::write(
        sdir.join("turns.jsonl"),
        format!("{}\n", serde_json::to_string(&envelope).unwrap()),
    )
    .unwrap();
    (root, id)
}

fn opts(cwd: Option<PathBuf>, claude: &Path, codex: &Path) -> ForkOptions {
    ForkOptions {
        cwd,
        no_launch: true,
        claude_config_dir: Some(claude.to_path_buf()),
        codex_home: Some(codex.to_path_buf()),
    }
}

#[test]
fn fork_missing_cwd_fails_before_write() {
    let cfg = tmp();
    let (root, id) = fixture_vault(
        "claude-code",
        &sample_messages(),
        Some("/nonexistent/dir/xyz"),
    );
    let err = fork::fork(
        root.path(),
        &id,
        Target::Claude,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not exist"));
    assert!(walk_files(cfg.path()).is_empty(), "nothing may be written");
}

#[test]
fn fork_cwd_override_wins() {
    let cfg = tmp();
    let workdir = tmp();
    let (root, id) = fixture_vault(
        "claude-code",
        &sample_messages(),
        Some("/nonexistent/dir/xyz"),
    );
    let out = fork::fork(
        root.path(),
        &id,
        Target::Claude,
        &opts(Some(workdir.path().to_path_buf()), cfg.path(), cfg.path()),
    )
    .unwrap();
    assert!(out.path.exists());
    assert_eq!(out.launch[0], "claude");
    assert_eq!(out.launch[2], out.new_id);
}

#[test]
fn fork_same_harness_claude_roundtrip() {
    let cfg = tmp();
    let workdir = tmp();
    let msgs = sample_messages();
    let (root, id) = fixture_vault(
        "claude-code",
        &msgs,
        Some(&workdir.path().to_string_lossy()),
    );
    let out = fork::fork(
        root.path(),
        &id,
        Target::Claude,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap();
    let recs = read_jsonl(&out.path);
    let contents: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "user" || r["type"] == "assistant")
        .map(|r| &r["message"]["content"])
        .collect();
    let expected: Vec<Value> = msgs
        .iter()
        .map(|m| claude_writer::strip_cache_control(m["content"].clone()))
        .collect();
    assert_eq!(contents.len(), expected.len());
    for (got, exp) in contents.iter().zip(&expected) {
        assert_eq!(*got, exp);
    }
}

#[test]
fn fork_cross_harness_codex_to_claude() {
    let cfg = tmp();
    let workdir = tmp();
    let mut history = vec![json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "scaffolding"}]})];
    history.extend(codex_items());
    history.push(json!({"type": "custom_tool_call", "name": "totally_unknown", "input": "raw", "call_id": "call_9"}));
    history.push(json!({"type": "custom_tool_call_output", "call_id": "call_9", "output": "res", "status": "completed"}));
    let (root, id) = fixture_vault("codex", &history, Some(&workdir.path().to_string_lossy()));
    let out = fork::fork(
        root.path(),
        &id,
        Target::Claude,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap();
    let recs = read_jsonl(&out.path);
    let text = std::fs::read_to_string(&out.path).unwrap();
    // shell mapped to Bash; unknown tool degraded to text, fork did not fail.
    assert!(text.contains("\"name\":\"Bash\""));
    assert!(text.contains("totally_unknown"));
    // valid chain
    let conv: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "user" || r["type"] == "assistant")
        .collect();
    assert!(conv[0]["parentUuid"].is_null());
    for w in conv.windows(2) {
        assert_eq!(w[1]["parentUuid"], w[0]["uuid"]);
    }
}

#[test]
fn fork_cross_harness_claude_to_codex() {
    let cfg = tmp();
    let workdir = tmp();
    let msgs = sample_messages();
    let (root, id) = fixture_vault(
        "claude-code",
        &msgs,
        Some(&workdir.path().to_string_lossy()),
    );
    let out = fork::fork(
        root.path(),
        &id,
        Target::Codex,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap();
    let recs = read_jsonl(&out.path);
    assert_eq!(recs[0]["type"], "session_meta");
    let text = std::fs::read_to_string(&out.path).unwrap();
    assert!(text.contains("\"name\":\"shell\""));
    assert_eq!(out.launch[0], "codex");
    // filename uuid == session_meta id == launch id
    let fname = out.path.file_stem().unwrap().to_string_lossy().to_string();
    assert!(fname.ends_with(&out.new_id));
    assert_eq!(recs[0]["payload"]["id"].as_str().unwrap(), out.new_id);
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

// ---------- integration smoke against the real vault (read-only) ----------

fn real_vault() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let root = PathBuf::from(home).join(".dotfiles/vault/sessions");
    root.join(".meta").is_dir().then_some(root)
}

fn pick_session(root: &Path, harness: &str) -> Option<vaultr::vault::Session> {
    vaultr::vault::list_sessions(root)
        .ok()?
        .into_iter()
        .find(|s| {
            s.meta.harness.as_deref() == Some(harness)
                && s.meta.cwd.as_deref().is_some_and(|c| Path::new(c).is_dir())
                && vaultr::vault::session_dir(root, s).is_ok()
        })
}

fn smoke(harness: &str, target: Target) {
    let Some(root) = real_vault() else {
        eprintln!("skipping: no real vault on this machine");
        return;
    };
    let Some(session) = pick_session(&root, harness) else {
        eprintln!("skipping: no {harness} session with live cwd");
        return;
    };
    let cfg = tmp();
    let out = match fork::fork(
        &root,
        &session.id,
        target,
        &opts(None, cfg.path(), cfg.path()),
    ) {
        Ok(o) => o,
        Err(e) if e.to_string().contains("empty history") => return,
        Err(e) => panic!("fork failed: {e:#}"),
    };
    let recs = read_jsonl(&out.path);
    assert!(!recs.is_empty());
    match target {
        Target::Claude => {
            let conv: Vec<&Value> = recs
                .iter()
                .filter(|r| r["type"] == "user" || r["type"] == "assistant")
                .collect();
            assert!(!conv.is_empty());
            assert!(conv[0]["parentUuid"].is_null());
            for w in conv.windows(2) {
                assert_eq!(w[1]["parentUuid"], w[0]["uuid"]);
            }
            assert_eq!(recs.last().unwrap()["type"], "last-prompt");
        }
        Target::Codex => {
            assert_eq!(recs[0]["type"], "session_meta");
            assert_eq!(recs[0]["payload"]["id"].as_str().unwrap(), out.new_id);
            assert!(recs.iter().any(|r| r["type"] == "response_item"));
        }
    }
}

#[test]
fn smoke_claude_to_claude() {
    smoke("claude-code", Target::Claude);
}

#[test]
fn smoke_claude_to_codex() {
    smoke("claude-code", Target::Codex);
}

#[test]
fn smoke_codex_to_codex() {
    smoke("codex", Target::Codex);
}

#[test]
fn smoke_codex_to_claude() {
    smoke("codex", Target::Claude);
}
