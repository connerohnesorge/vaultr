//! Fork writer tests. All writes go to temp dirs — never the real ~/.claude,
//! ~/.codex, or the vault. Integration smokes read the real vault read-only
//! and skip when it is absent.

use chrono::{Local, TimeZone, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use vaultr::fork::{self, ForkOptions, Target};
use vaultr::{claude_writer, codex_writer, normalize, pi_writer, recon};

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
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
            {"type": "tool_use", "id": "toolu_abc", "name": "Bash",
             "cache_control": {"type": "ephemeral"}, "input": {
                "command": "ls",
                "cache_control": {"opaque": ["nested", 7]},
                "payload": {"cache_control": {"keep": "deeper"}}
            }}
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

// Mirror the writer's stored-form normalization: cache_control stripped, and a
// user message whose content is a single bare text block collapses to a string.
fn stored_form(m: &Value) -> Value {
    let mut v = m.clone();
    if let Some(blocks) = v["content"].as_array_mut() {
        for block in blocks {
            if let Some(block) = block.as_object_mut() {
                block.remove("cache_control");
            }
        }
    }
    let is_user = v["role"] == "user";
    let content = &v["content"];
    if is_user {
        if let Some(a) = content.as_array() {
            if a.len() == 1
                && a[0]["type"] == "text"
                && a[0].as_object().map(|o| o.len()) == Some(2)
            {
                let mut v2 = v.clone();
                v2["content"] = a[0]["text"].clone();
                return v2;
            }
        }
    }
    v
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
    let expected: Vec<Value> = msgs.iter().map(stored_form).collect();
    for (rec, exp) in conv.iter().zip(&expected) {
        assert_eq!(rec["message"]["content"], exp["content"]);
        assert_eq!(rec["message"]["role"], exp["role"]);
    }
    // system-reminder text preserved byte-for-byte in the stored (collapsed) record.
    let first = conv[0]["message"]["content"].as_str().unwrap();
    assert!(first.contains("<system-reminder>secret scaffolding</system-reminder>"));
    assert!(conv[1]["message"]["content"][0]
        .get("cache_control")
        .is_none());
    assert!(conv[1]["message"]["content"][1]
        .get("cache_control")
        .is_none());
    assert_eq!(
        conv[1]["message"]["content"][1]["input"], msgs[1]["content"][1]["input"],
        "opaque tool input must remain byte-identical"
    );
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
        None,
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
    let (id, path) = codex_writer::write(home.path(), "/tmp", None, &items, None, None).unwrap();
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
        if orig["type"] == "reasoning" {
            // reasoning is normalized to the replayed wire shape
            assert_eq!(w["id"], orig["id"]);
            assert_eq!(w["summary"], orig["summary"]);
            assert_eq!(w["content"], Value::Null);
            assert_eq!(w["encrypted_content"], orig["encrypted_content"]);
        } else {
            assert_eq!(*w, orig, "response_item must round-trip verbatim");
        }
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
    codex_writer::write_with_id(home.path(), "/", None, &items, None, None, &id, start).unwrap();
    let err = codex_writer::write_with_id(home.path(), "/", None, &items, None, None, &id, start)
        .unwrap_err();
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

#[test]
fn codex_passthrough_only_lifts_one_truly_leading_developer_message() {
    let developer = |text: &str| {
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": text}]})
    };
    let user = json!({"type": "message", "role": "user",
                      "content": [{"type": "input_text", "text": "user"}]});
    let tools = json!({"type": "additional_tools", "role": "developer"});

    let (items, base) = fork::prepare_codex_passthrough(&[developer("first")]);
    assert!(items.is_empty());
    assert_eq!(base.as_deref(), Some("first"));

    let (items, base) = fork::prepare_codex_passthrough(&[user.clone(), developer("later")]);
    assert_eq!(items, vec![user.clone(), developer("later")]);
    assert_eq!(base, None);

    let (items, base) = fork::prepare_codex_passthrough(&[
        tools.clone(),
        user.clone(),
        tools,
        developer("still conversational"),
    ]);
    assert_eq!(items, vec![user, developer("still conversational")]);
    assert_eq!(base, None, "removed scaffolding must not move the boundary");

    let (items, base) = fork::prepare_codex_passthrough(&[developer("first"), developer("second")]);
    assert_eq!(items, vec![developer("second")]);
    assert_eq!(base.as_deref(), Some("first"));
}

// ---------- Pi writer ----------

#[test]
fn pi_v3_path_chain_and_tool_pairs() {
    let root = tmp();
    let id = uuid::Uuid::now_v7().to_string();
    let started = Utc.with_ymd_and_hms(2026, 7, 3, 23, 58, 7).unwrap();
    let messages = normalize::normalize(&sample_messages());
    let path = pi_writer::write_with_id(
        root.path(),
        "/Users/x/.dotfiles",
        "anthropic",
        "anthropic-messages",
        Some("claude-opus-4-8"),
        &messages,
        &id,
        started,
    )
    .unwrap();
    assert_eq!(
        path.strip_prefix(root.path()).unwrap(),
        PathBuf::from(format!(
            "--Users-x-.dotfiles--/2026-07-03T23-58-07-000Z_{id}.jsonl"
        ))
    );

    let recs = read_jsonl(&path);
    assert_eq!(recs[0]["type"], "session");
    assert_eq!(recs[0]["version"], 3);
    assert_eq!(recs[0]["id"], id);
    assert_eq!(recs[0]["cwd"], "/Users/x/.dotfiles");
    assert_eq!(recs[1]["type"], "model_change");
    assert_eq!(recs[1]["provider"], "anthropic");
    assert_eq!(recs[1]["modelId"], "claude-opus-4-8");

    let entries = &recs[1..];
    assert!(entries[0]["parentId"].is_null());
    for window in entries.windows(2) {
        assert_eq!(window[1]["parentId"], window[0]["id"]);
    }
    assert!(entries.iter().all(|entry| {
        entry["id"]
            .as_str()
            .is_some_and(|id| id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()))
    }));

    let messages: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .map(|entry| &entry["message"])
        .collect();
    assert_eq!(
        messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "toolResult", "assistant"]
    );
    assert!(!messages[0].to_string().contains("secret scaffolding"));
    let call = messages[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "toolCall")
        .unwrap();
    assert_eq!(call["name"], "bash");
    assert_eq!(call["arguments"]["command"], "ls");
    assert_eq!(messages[2]["toolCallId"], call["id"]);
    assert_eq!(messages[2]["toolName"], "bash");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn pi_collision_is_refused() {
    let root = tmp();
    let id = uuid::Uuid::now_v7().to_string();
    let started = Utc.with_ymd_and_hms(2026, 7, 3, 23, 58, 7).unwrap();
    let messages = normalize::normalize(&sample_messages());
    let write = || {
        pi_writer::write_with_id(
            root.path(),
            "/tmp",
            "anthropic",
            "anthropic-messages",
            None,
            &messages,
            &id,
            started,
        )
    };
    let path = write().unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(write()
        .unwrap_err()
        .to_string()
        .contains("refusing to overwrite"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
}

// ---------- fork() end-to-end over a fixture vault ----------

fn fixture_vault(
    harness: &str,
    history: &[Value],
    cwd: Option<&str>,
) -> (tempfile::TempDir, String) {
    fixture_vault_with(harness, harness, history, cwd)
}

/// Like `fixture_vault`, but lets the sidecar meta.harness disagree with the
/// harness recorded on the wire envelopes.
fn fixture_vault_with(
    meta_harness: &str,
    env_harness: &str,
    history: &[Value],
    cwd: Option<&str>,
) -> (tempfile::TempDir, String) {
    let root = tmp();
    let id = if env_harness == "codex" {
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
            "harness": meta_harness,
            "session_id": id,
            "cwd": cwd,
            "git_branch": "main",
            "model": if env_harness == "codex" { "gpt-5.6-sol" } else { "claude-opus-4-8" },
            "original_start": "2026-07-01T00:00:00.000Z",
            "last_observation": "2026-07-01T00:00:00.000Z",
        }))
        .unwrap(),
    )
    .unwrap();
    let sdir = root.path().join("2026/07/01").join(&id);
    std::fs::create_dir_all(&sdir).unwrap();
    let key = if env_harness == "codex" {
        "input"
    } else {
        "messages"
    };
    let envelope = json!({
        "schema_version": 1,
        "harness": env_harness,
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
        pi_session_dir: Some(codex.to_path_buf()),
        ..Default::default()
    }
}

fn set_fixture_model(root: &Path, id: &str, model: &str) {
    let path = root.join(".meta").join(format!("{id}.json"));
    let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    meta["model"] = json!(model);
    std::fs::write(path, serde_json::to_string(&meta).unwrap()).unwrap();
}

fn fork_cli(
    root: &Path,
    id: &str,
    target: Target,
    cwd: &Path,
    sandbox: &Path,
    home: Option<&str>,
    config: Option<(&str, &str)>,
) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_vaultr"));
    command
        .current_dir(sandbox)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .arg("--vault")
        .arg(root)
        .arg("session")
        .arg("fork")
        .arg(id)
        .arg("--into")
        .arg(match target {
            Target::Claude => "claude",
            Target::Codex => "codex",
            Target::Pi => "pi",
        })
        .arg("--cwd")
        .arg(cwd)
        .arg("--no-launch");
    match home {
        Some(home) => {
            command.env("HOME", home);
        }
        None => {
            command.env_remove("HOME");
        }
    }
    if let Some((name, value)) = config {
        command.env(name, value);
    }
    command.output().unwrap()
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
fn fork_canonicalizes_relative_and_symlink_cwds() {
    let cfg = tmp();
    let current = std::env::current_dir().unwrap();
    let relative = tempfile::Builder::new()
        .prefix("vaultr-relative-")
        .tempdir_in(&current)
        .unwrap();
    let relative_path = relative.path().strip_prefix(&current).unwrap();
    let (root, id) = fixture_vault("claude-code", &sample_messages(), None);
    let canonical = relative.path().canonicalize().unwrap();
    for target in [Target::Claude, Target::Codex, Target::Pi] {
        let out = fork::fork(
            root.path(),
            &id,
            target,
            &opts(Some(relative_path.to_path_buf()), cfg.path(), cfg.path()),
        )
        .unwrap();
        assert_eq!(out.cwd, canonical);
        match target {
            Target::Claude => assert_eq!(
                out.path.parent().unwrap().file_name().unwrap(),
                claude_writer::encode_project_dir(&canonical.to_string_lossy()).as_str()
            ),
            Target::Codex => assert_eq!(
                read_jsonl(&out.path)[0]["payload"]["cwd"],
                canonical.to_string_lossy().as_ref()
            ),
            Target::Pi => {
                assert_eq!(
                    read_jsonl(&out.path)[0]["cwd"],
                    canonical.to_string_lossy().as_ref()
                )
            }
        }
    }

    #[cfg(unix)]
    {
        let real = tmp();
        let links = tmp();
        let link = links.path().join("linked-cwd");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let (root, id) = fixture_vault("claude-code", &sample_messages(), None);
        let canonical = real.path().canonicalize().unwrap();
        for target in [Target::Claude, Target::Codex, Target::Pi] {
            let out = fork::fork(
                root.path(),
                &id,
                target,
                &opts(Some(link.clone()), cfg.path(), cfg.path()),
            )
            .unwrap();
            assert_eq!(out.cwd, canonical);
            match target {
                Target::Claude => assert_eq!(
                    out.path.parent().unwrap().file_name().unwrap(),
                    claude_writer::encode_project_dir(&canonical.to_string_lossy()).as_str()
                ),
                Target::Codex => assert_eq!(
                    read_jsonl(&out.path)[0]["payload"]["cwd"],
                    canonical.to_string_lossy().as_ref()
                ),
                Target::Pi => assert_eq!(
                    read_jsonl(&out.path)[0]["cwd"],
                    canonical.to_string_lossy().as_ref()
                ),
            }
        }
    }
}

#[test]
fn fork_rejects_missing_home_and_relative_config_roots_before_write() {
    let workdir = tmp();
    let (root, id) = fixture_vault("claude-code", &sample_messages(), None);
    for target in [Target::Claude, Target::Codex, Target::Pi] {
        for home in [None, Some("")] {
            let sandbox = tmp();
            let output = fork_cli(
                root.path(),
                &id,
                target,
                workdir.path(),
                sandbox.path(),
                home,
                None,
            );
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("HOME is missing or empty"));
            assert!(
                walk_files(sandbox.path()).is_empty(),
                "nothing may be written"
            );
        }

        let sandbox = tmp();
        let (name, value) = match target {
            Target::Claude => ("CLAUDE_CONFIG_DIR", "relative-claude"),
            Target::Codex => ("CODEX_HOME", "relative-codex"),
            Target::Pi => ("PI_CODING_AGENT_SESSION_DIR", "relative-pi"),
        };
        let output = fork_cli(
            root.path(),
            &id,
            target,
            workdir.path(),
            sandbox.path(),
            Some("/"),
            Some((name, value)),
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be absolute"));
        assert!(
            walk_files(sandbox.path()).is_empty(),
            "nothing may be written"
        );

        let sandbox = tmp();
        let config = sandbox.path().join("absolute-config");
        let config_text = config.to_string_lossy();
        let output = fork_cli(
            root.path(),
            &id,
            target,
            workdir.path(),
            sandbox.path(),
            None,
            Some((name, &config_text)),
        );
        assert!(output.status.success(), "{output:#?}");
        assert!(!walk_files(&config).is_empty());
    }
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
    set_fixture_model(root.path(), &id, "claude-source-model");
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
        .map(|m| stored_form(m)["content"].clone())
        .collect();
    assert_eq!(contents.len(), expected.len());
    for (got, exp) in contents.iter().zip(&expected) {
        assert_eq!(*got, exp);
    }
    assert!(recs
        .iter()
        .filter(|r| r["type"] == "assistant")
        .all(|r| r["message"]["model"] == "claude-source-model"));
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
    set_fixture_model(root.path(), &id, "codex-source-model");
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
    assert!(recs
        .iter()
        .filter(|r| r["type"] == "assistant")
        .all(|r| r["message"]["model"] == "claude-opus-4-8"));
    assert!(!text.contains("codex-source-model"));
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
    set_fixture_model(root.path(), &id, "claude-source-model");
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
    let turn_context = recs.iter().find(|r| r["type"] == "turn_context").unwrap();
    assert_eq!(turn_context["payload"]["model"], "gpt-5.6-sol");
    assert_eq!(
        turn_context["payload"]["collaboration_mode"]["settings"]["model"],
        "gpt-5.6-sol"
    );
    assert!(!text.contains("claude-source-model"));
    assert_eq!(out.launch[0], "codex");
    // filename uuid == session_meta id == launch id
    let fname = out.path.file_stem().unwrap().to_string_lossy().to_string();
    assert!(fname.ends_with(&out.new_id));
    assert_eq!(recs[0]["payload"]["id"].as_str().unwrap(), out.new_id);
}

#[test]
fn fork_codex_to_pi_uses_native_v3_history() {
    let cfg = tmp();
    let workdir = tmp();
    let (root, id) = fixture_vault(
        "codex",
        &codex_items(),
        Some(&workdir.path().to_string_lossy()),
    );
    let out = fork::fork(
        root.path(),
        &id,
        Target::Pi,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap();
    let recs = read_jsonl(&out.path);
    assert_eq!(recs[0]["type"], "session");
    assert_eq!(recs[0]["version"], 3);
    assert_eq!(recs[0]["id"], out.new_id);
    assert_eq!(recs[1]["type"], "model_change");
    assert_eq!(recs[1]["provider"], "openai-codex");
    assert_eq!(recs[1]["modelId"], "gpt-5.6-sol");
    assert!(recs.iter().any(|entry| {
        entry["message"]["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["name"] == "bash"))
    }));
    assert_eq!(
        out.launch,
        [
            "pi".to_string(),
            "--session".to_string(),
            out.path.to_string_lossy().into_owned()
        ]
    );
}

#[test]
fn fork_prompt_and_read_only_flags_are_target_native() {
    let cfg = tmp();
    let workdir = tmp();
    let (root, id) = fixture_vault(
        "claude-code",
        &sample_messages(),
        Some(&workdir.path().to_string_lossy()),
    );
    for target in [Target::Claude, Target::Codex, Target::Pi] {
        let mut options = opts(None, cfg.path(), cfg.path());
        options.prompt = Some("review this".into());
        options.read_only = true;
        let out = fork::fork(root.path(), &id, target, &options).unwrap();
        match target {
            Target::Claude => assert_eq!(
                out.launch,
                [
                    "claude",
                    "--permission-mode",
                    "plan",
                    "--tools",
                    "Read,Grep,Glob",
                    "--resume",
                    &out.new_id,
                    "review this",
                ]
            ),
            Target::Codex => assert_eq!(
                out.launch,
                [
                    "codex",
                    "resume",
                    "--sandbox",
                    "read-only",
                    "--ask-for-approval",
                    "never",
                    &out.new_id,
                    "review this",
                ]
            ),
            Target::Pi => assert_eq!(
                out.launch,
                [
                    "pi",
                    "--tools",
                    "read,grep,find,ls",
                    "--session",
                    out.path.to_string_lossy().as_ref(),
                    "review this",
                ]
            ),
        }
    }
}

#[test]
fn fork_envelope_harness_outranks_stale_meta() {
    // The crack this guards: envelopes say codex but the mutable sidecar says
    // claude — the fork must still take the codex passthrough path, not the
    // claude branch (which would translate the items and corrupt the fork).
    let cfg = tmp();
    let workdir = tmp();
    let mut history = vec![
        json!({"type": "additional_tools", "role": "developer"}),
        json!({"type": "message", "role": "developer",
               "content": [{"type": "input_text", "text": "You are Codex, an agent based on GPT-5. ..."}]}),
    ];
    history.extend(codex_items());
    let (root, id) = fixture_vault_with(
        "claude",
        "codex",
        &history,
        Some(&workdir.path().to_string_lossy()),
    );
    set_fixture_model(root.path(), &id, "codex-source-model");
    let out = fork::fork(
        root.path(),
        &id,
        Target::Codex,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap();
    let recs = read_jsonl(&out.path);
    // Passthrough proof: base instructions lifted into session_meta (the
    // translate path never sets them) …
    assert_eq!(
        recs[0]["payload"]["base_instructions"]["text"],
        "You are Codex, an agent based on GPT-5. ..."
    );
    let turn_context = recs.iter().find(|r| r["type"] == "turn_context").unwrap();
    assert_eq!(turn_context["payload"]["model"], "codex-source-model");
    assert_eq!(
        turn_context["payload"]["collaboration_mode"]["settings"]["model"],
        "codex-source-model"
    );
    // … and the opaque reasoning item survives verbatim (translate drops it).
    let reasoning: Vec<&Value> = recs
        .iter()
        .filter(|r| r["type"] == "response_item" && r["payload"]["type"] == "reasoning")
        .collect();
    assert_eq!(reasoning.len(), 1);
    assert_eq!(reasoning[0]["payload"]["encrypted_content"], "OPAQUE==");
}

#[test]
fn fork_conflicting_explicit_harnesses_fails_before_write() {
    let cfg = tmp();
    let workdir = tmp();
    let (root, id) = fixture_vault(
        "claude-code",
        &sample_messages(),
        Some(&workdir.path().to_string_lossy()),
    );
    let capture = root.path().join("2026/07/01").join(&id).join("turns.jsonl");
    let conflict = json!({
        "schema_version": 1,
        "harness": "codex",
        "request": {"body_delta": {"history": {
            "key": "input", "prefix_length": 4, "append": []
        }}},
        "response": {"complete": false}
    });
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(capture)
            .unwrap(),
        "{conflict}"
    )
    .unwrap();

    let err = fork::fork(
        root.path(),
        &id,
        Target::Claude,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("conflicting explicit harness labels"));
    assert!(walk_files(cfg.path()).is_empty(), "nothing may be written");
}

#[test]
fn fork_impossible_prefix_fails_before_write() {
    let cfg = tmp();
    let workdir = tmp();
    let (root, id) = fixture_vault(
        "claude-code",
        &sample_messages(),
        Some(&workdir.path().to_string_lossy()),
    );
    let capture = root.path().join("2026/07/01").join(&id).join("turns.jsonl");
    std::fs::write(
        capture,
        format!(
            "{}\n",
            json!({
                "schema_version": 1,
                "harness": "claude-code",
                "request": {"body_delta": {"history": {
                    "key": "messages", "prefix_length": 1,
                    "append": [{"role": "user", "content": "CAPTURE_SECRET"}]
                }}},
                "response": {"complete": false}
            })
        ),
    )
    .unwrap();

    let err = fork::fork(
        root.path(),
        &id,
        Target::Claude,
        &opts(None, cfg.path(), cfg.path()),
    )
    .unwrap_err();
    assert!(!format!("{err:#}").contains("CAPTURE_SECRET"));
    assert!(walk_files(cfg.path()).is_empty(), "nothing may be written");
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
                && vaultr::vault::session_dir(root, s)
                    .and_then(|dir| vaultr::vault::capture_file(&dir))
                    .and_then(|capture| recon::reconstruct(&capture))
                    .is_ok_and(|recon| !recon.messages.is_empty())
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
                .filter(|r| {
                    r["type"] == "user" || r["type"] == "assistant" || r["type"] == "attachment"
                })
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
        Target::Pi => {
            assert_eq!(recs[0]["type"], "session");
            assert_eq!(recs[0]["id"].as_str().unwrap(), out.new_id);
            assert!(recs.iter().any(|r| r["type"] == "message"));
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

#[test]
fn smoke_claude_to_pi() {
    smoke("claude-code", Target::Pi);
}

#[test]
fn smoke_codex_to_pi() {
    smoke("codex", Target::Pi);
}
