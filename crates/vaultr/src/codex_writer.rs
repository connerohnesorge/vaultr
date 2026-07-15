//! Native Codex CLI rollout writer: emits a resumable
//! `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-<local-time>-<uuidv7>.jsonl`
//! per the verified 0.144.4 format (docs/native-formats.md). Input is a list
//! of OpenAI Responses items, either passed through from a same-harness
//! reconstruction or produced by cross-harness translation. The Codex state
//! database is never touched — read repair discovers the rollout by filename.

use crate::writeio::{truncate_chars, write_atomic_0600};
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Local, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const CODEX_CLI_VERSION: &str = "0.144.4";
const DEFAULT_BASE_INSTRUCTIONS: &str = "You are Codex, an agent based on GPT-5.";

/// Write a rollout with a fresh UUIDv7 timestamped now. Returns (id, path).
pub fn write(
    codex_home: &Path,
    cwd: &str,
    git_branch: Option<&str>,
    items: &[Value],
    base_instructions: Option<&str>,
    model: Option<&str>,
) -> Result<(String, PathBuf)> {
    let id = uuid::Uuid::now_v7().to_string();
    let path = write_with_id(
        codex_home,
        cwd,
        git_branch,
        items,
        base_instructions,
        model,
        &id,
        Local::now(),
    )?;
    Ok((id, path))
}

/// IANA timezone name for turn_context (e.g. "America/Chicago").
fn iana_timezone() -> String {
    // /etc/localtime is a symlink into the zoneinfo db on macOS and Linux.
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().into_owned();
            s.split("zoneinfo/").nth(1).map(String::from)
        })
        .unwrap_or_else(|| "UTC".into())
}

/// Write a rollout under an explicit id and start time (exposed for tests).
#[allow(clippy::too_many_arguments)] // ponytail: test seam, not a public API worth a params struct
pub fn write_with_id(
    codex_home: &Path,
    cwd: &str,
    git_branch: Option<&str>,
    items: &[Value],
    base_instructions: Option<&str>,
    meta_model: Option<&str>,
    session_id: &str,
    start_local: DateTime<Local>,
) -> Result<PathBuf> {
    if items.is_empty() {
        bail!("nothing to fork: reconstructed history is empty");
    }
    // Local time for the directory date and filename; UTC inside records.
    let dir = codex_home
        .join("sessions")
        .join(start_local.format("%Y").to_string())
        .join(start_local.format("%m").to_string())
        .join(start_local.format("%d").to_string());
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!(
        "rollout-{}-{session_id}.jsonl",
        start_local.format("%Y-%m-%dT%H-%M-%S")
    ));

    let start_utc = start_local.with_timezone(&Utc);
    let mut ts = start_utc;
    let iso = |t: DateTime<Utc>| t.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut meta = json!({
        "session_id": session_id,
        "id": session_id,
        "timestamp": iso(start_utc),
        "cwd": cwd,
        "originator": "codex-tui",
        "cli_version": CODEX_CLI_VERSION,
        "source": "cli",
        "thread_source": "user",
        "model_provider": "openai",
        "base_instructions": {"text": base_instructions.unwrap_or(DEFAULT_BASE_INSTRUCTIONS)},
        "history_mode": "legacy",
        "context_window": {"window_id": uuid::Uuid::now_v7().to_string()},
    });
    if let Some(b) = git_branch {
        meta["git"] = json!({"branch": b});
    }
    let mut lines = vec![serde_json::to_string(&json!({
        "timestamp": iso(ts),
        "type": "session_meta",
        "payload": meta,
    }))?];

    // A stored world_state tells resume the AGENTS.md instructions were already
    // applied; without it Codex injects a fresh "# AGENTS.md instructions" user
    // message, breaking byte-identity with a native resume. The text is
    // recovered from the history's own injected block.
    if let Some((agents_dir, agents_text)) = find_agents_md(items) {
        ts += Duration::milliseconds(1);
        lines.push(serde_json::to_string(&json!({
            "timestamp": iso(ts),
            "type": "world_state",
            "payload": {
                "full": true,
                "state": {
                    "agents_md": {"directory": agents_dir, "text": agents_text},
                    "apps_instructions": true,
                    "plugins_instructions": true,
                    "skills": {"includeInstructions": true},
                    "environments": {
                        "environments": {"local": {"cwd": cwd, "status": "available", "shell": "zsh"}},
                        "current_date": start_local.format("%Y-%m-%d").to_string(),
                        "timezone": iana_timezone(),
                        "filesystem": format!("<filesystem><workspace_roots><root>{cwd}</root></workspace_roots><permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile></filesystem>"),
                    },
                },
            },
        }))?);
    }

    // A stored turn_context is what resume compares its own settings against;
    // without one Codex re-injects <permissions instructions> (and friends)
    // into the request, breaking byte-identity with a native resume.
    // ponytail: values are this machine's observed 0.144.4 defaults — if the
    // user's config diverges, resume re-injects once and the gate diff shows it.
    ts += Duration::milliseconds(1);
    let model = meta_model.unwrap_or("gpt-5.6-sol");
    lines.push(serde_json::to_string(&json!({
        "timestamp": iso(ts),
        "type": "turn_context",
        "payload": {
            "turn_id": uuid::Uuid::now_v7().to_string(),
            "cwd": cwd,
            "workspace_roots": [cwd],
            "current_date": start_local.format("%Y-%m-%d").to_string(),
            "timezone": iana_timezone(),
            "approval_policy": "never",
            "approvals_reviewer": "user",
            "sandbox_policy": {"type": "danger-full-access"},
            "permission_profile": {"type": "disabled"},
            "model": model,
            "comp_hash": "3000",
            "personality": "pragmatic",
            "collaboration_mode": {"mode": "default", "settings": {"model": model, "reasoning_effort": "low", "developer_instructions": null}},
            "multi_agent_version": "v2",
            "multi_agent_mode": "explicitRequestOnly",
            "realtime_active": false,
            "effort": "low",
            "summary": "auto",
        },
    }))?);

    for item in items {
        ts += Duration::milliseconds(200);
        let item = normalize_reasoning(item);
        lines.push(serde_json::to_string(&json!({
            "timestamp": iso(ts),
            "type": "response_item",
            "payload": item,
        }))?);
        // Emit the user_message preview event the resume picker / DB backfill
        // derives titles from — for real typed user messages only.
        if let Some(text) = typed_user_text(&item) {
            ts += Duration::milliseconds(1);
            lines.push(serde_json::to_string(&json!({
                "timestamp": iso(ts),
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": truncate_chars(&text, 4000),
                    "images": [],
                    "local_images": [],
                    "text_elements": [],
                },
            }))?);
        }
    }

    write_atomic_0600(&dest, &lines)?;
    Ok(dest)
}

/// Codex serializes replayed reasoning items as
/// {"type","summary","content":null,"encrypted_content"} — but a live-stream
/// `output_item.done` (the reconstructor's trailing response) omits the null
/// `content`. Normalize to the replayed wire shape so a resumed fork's bytes
/// match a native resume. Other item types pass through untouched.
fn normalize_reasoning(item: &Value) -> Value {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return item.clone();
    }
    let mut out = serde_json::Map::new();
    out.insert("type".into(), json!("reasoning"));
    if let Some(id) = item.get("id") {
        out.insert("id".into(), id.clone());
    }
    out.insert(
        "summary".into(),
        item.get("summary").cloned().unwrap_or(json!([])),
    );
    out.insert(
        "content".into(),
        item.get("content").cloned().unwrap_or(Value::Null),
    );
    if let Some(ec) = item.get("encrypted_content") {
        out.insert("encrypted_content".into(), ec.clone());
    }
    // keep any remaining keys (e.g. turn metadata) verbatim, after the
    // canonical ones
    if let Some(o) = item.as_object() {
        for (k, v) in o {
            if !out.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Recover the applied AGENTS.md (directory, text) from the injected
/// "# AGENTS.md instructions for <dir>\n\n<INSTRUCTIONS>\n<text>\n</INSTRUCTIONS>"
/// block Codex placed in the history.
fn find_agents_md(items: &[Value]) -> Option<(String, String)> {
    for item in items {
        let Some(blocks) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for b in blocks {
            let Some(t) = b.get("text").and_then(Value::as_str) else {
                continue;
            };
            let Some(rest) = t.strip_prefix("# AGENTS.md instructions for ") else {
                continue;
            };
            if let Some((dir, body)) = rest.split_once("\n\n<INSTRUCTIONS>\n") {
                if let Some(text) = body.strip_suffix("\n</INSTRUCTIONS>") {
                    return Some((dir.to_string(), text.to_string()));
                }
            }
        }
    }
    None
}

/// Text of a user message item that looks typed by a human (not injected
/// scaffolding like AGENTS.md content, environment context, or reminders).
fn typed_user_text(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("message")
        || item.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let text: String = item
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    const INJECTED: &[&str] = &[
        "# AGENTS.md",
        "<permissions",
        "<environment_context",
        "<user_instructions",
        "<system-reminder",
        "<INSTRUCTIONS",
        "Caveat:",
    ];
    if trimmed.is_empty() || INJECTED.iter().any(|p| trimmed.starts_with(p)) {
        return None;
    }
    Some(trimmed.to_string())
}
