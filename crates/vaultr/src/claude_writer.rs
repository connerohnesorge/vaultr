//! Native Claude Code session writer: emits a resumable
//! `<config>/projects/<encoded-cwd>/<uuid>.jsonl` per the verified 2.1.210
//! format (docs/native-formats.md). Input is a list of Anthropic wire
//! messages ({"role","content"}), either passed through verbatim from a
//! same-harness reconstruction or produced by cross-harness translation.

use crate::writeio::write_atomic_0600;
use anyhow::{bail, Result};
use chrono::{Duration, SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const CLAUDE_VERSION: &str = "2.1.210";

/// Encode an absolute cwd into the Claude project directory name:
/// every non-alphanumeric character becomes '-'.
pub fn encode_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Write a session with a fresh UUIDv4. Returns (session_id, path).
pub fn write(
    config_dir: &Path,
    cwd: &str,
    git_branch: Option<&str>,
    model: Option<&str>,
    messages: &[Value],
) -> Result<(String, PathBuf)> {
    let id = uuid::Uuid::new_v4().to_string();
    let path = write_with_id(config_dir, cwd, git_branch, model, messages, &id)?;
    Ok((id, path))
}

/// Write a session under an explicit id (exposed for collision testing).
pub fn write_with_id(
    config_dir: &Path,
    cwd: &str,
    git_branch: Option<&str>,
    model: Option<&str>,
    messages: &[Value],
    session_id: &str,
) -> Result<PathBuf> {
    if messages.is_empty() {
        bail!("nothing to fork: reconstructed history is empty");
    }
    let project_dir = config_dir.join("projects").join(encode_project_dir(cwd));
    std::fs::create_dir_all(&project_dir)?;
    let dest = project_dir.join(format!("{session_id}.jsonl"));

    let model = model.unwrap_or("claude-opus-4-8");
    // Synthesize a recent monotonic timestamp sequence ending near now.
    let mut ts = Utc::now() - Duration::seconds(messages.len() as i64 + 2);
    let mut lines: Vec<String> = Vec::new();
    let mut parent: Option<String> = None;
    let mut first_user_text: Option<String> = None;

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = strip_cache_control(m.get("content").cloned().unwrap_or(Value::Null));
        let uuid = uuid::Uuid::new_v4().to_string();
        ts += Duration::seconds(1);
        let timestamp = ts.to_rfc3339_opts(SecondsFormat::Millis, true);

        let mut rec = Map::new();
        rec.insert(
            "parentUuid".into(),
            parent.clone().map_or(Value::Null, Value::String),
        );
        rec.insert("isSidechain".into(), json!(false));
        rec.insert("userType".into(), json!("external"));
        rec.insert("entrypoint".into(), json!("cli"));
        rec.insert("cwd".into(), json!(cwd));
        rec.insert("sessionId".into(), json!(session_id));
        rec.insert("version".into(), json!(CLAUDE_VERSION));
        if let Some(b) = git_branch {
            rec.insert("gitBranch".into(), json!(b));
        }
        if role == "assistant" {
            rec.insert("type".into(), json!("assistant"));
            rec.insert(
                "message".into(),
                json!({
                    "model": model,
                    "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    "type": "message",
                    "role": "assistant",
                    "content": content,
                    "stop_reason": if has_block(&content, "tool_use") { "tool_use" } else { "end_turn" },
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }),
            );
        } else {
            rec.insert("type".into(), json!("user"));
            rec.insert("promptId".into(), json!(uuid::Uuid::new_v4().to_string()));
            rec.insert(
                "message".into(),
                json!({"role": "user", "content": content}),
            );
            if first_user_text.is_none() {
                first_user_text = visible_user_text(&content);
            }
        }
        rec.insert("uuid".into(), json!(uuid));
        rec.insert("timestamp".into(), json!(timestamp));
        lines.push(serde_json::to_string(&Value::Object(rec))?);
        parent = Some(uuid);
    }

    // Trailing last-prompt so the resume picker shows a title and finds the leaf.
    let leaf = parent.expect("at least one record written");
    let prompt = crate::writeio::truncate_chars(&first_user_text.unwrap_or_default(), 200);
    lines.push(serde_json::to_string(&json!({
        "type": "last-prompt",
        "lastPrompt": prompt,
        "leafUuid": leaf,
        "sessionId": session_id,
    }))?);

    write_atomic_0600(&dest, &lines)?;
    Ok(dest)
}

fn has_block(content: &Value, block_type: &str) -> bool {
    content.as_array().is_some_and(|a| {
        a.iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some(block_type))
    })
}

/// First human-visible text of a user message, system-reminders stripped.
fn visible_user_text(content: &Value) -> Option<String> {
    let raw = match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(|b| {
            (b.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| b.get("text").and_then(Value::as_str).map(String::from))
                .flatten()
        }),
        _ => None,
    }?;
    let cleaned = crate::normalize::strip_system_reminders(&raw);
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Recursively remove request-time `cache_control` markers: the harness adds
/// them per request, so replaying them from records would break the
/// byte-identity of a resumed messages array.
pub fn strip_cache_control(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| k != "cache_control")
                .map(|(k, v)| (k, strip_cache_control(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_cache_control).collect()),
        other => other,
    }
}
