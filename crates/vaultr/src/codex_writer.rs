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
) -> Result<(String, PathBuf)> {
    let id = uuid::Uuid::now_v7().to_string();
    let path = write_with_id(
        codex_home,
        cwd,
        git_branch,
        items,
        base_instructions,
        &id,
        Local::now(),
    )?;
    Ok((id, path))
}

/// Write a rollout under an explicit id and start time (exposed for tests).
pub fn write_with_id(
    codex_home: &Path,
    cwd: &str,
    git_branch: Option<&str>,
    items: &[Value],
    base_instructions: Option<&str>,
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

    for item in items {
        ts += Duration::milliseconds(200);
        lines.push(serde_json::to_string(&json!({
            "timestamp": iso(ts),
            "type": "response_item",
            "payload": item,
        }))?);
        // Emit the user_message preview event the resume picker / DB backfill
        // derives titles from — for real typed user messages only.
        if let Some(text) = typed_user_text(item) {
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
