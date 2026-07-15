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
    let mut seen_first_user = false;

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let mut content = strip_cache_control(m.get("content").cloned().unwrap_or(Value::Null));
        if role == "user" && !seen_first_user {
            seen_first_user = true;
            content = strip_injected_context(content);
        }
        // Claude Code sends a freshly typed message as a one-element text-block
        // array but STORES it as a plain string, and replays the stored form on
        // resume — normalize to match, or the resumed bytes diverge.
        if role == "user" {
            content = collapse_single_text(content);
        }
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
        if role == "system" {
            // Wire system messages are re-rendered per request from stored
            // attachment records as "<hookName> hook success: <content>".
            // Invert that: one attachment carrying the full rendered text
            // reproduces the message byte-for-byte on resume.
            let text = content.as_str().map(String::from).unwrap_or_else(|| {
                content
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default()
            });
            if let Some(attachments) = parse_system_blob(&text) {
                for att in attachments {
                    let auuid = uuid::Uuid::new_v4().to_string();
                    let mut arec = rec.clone();
                    arec.insert(
                        "parentUuid".into(),
                        parent.clone().map_or(Value::Null, Value::String),
                    );
                    arec.insert("type".into(), json!("attachment"));
                    arec.insert("attachment".into(), att);
                    arec.insert("uuid".into(), json!(auuid));
                    arec.insert("timestamp".into(), json!(timestamp));
                    lines.push(serde_json::to_string(&Value::Object(arec))?);
                    parent = Some(auuid);
                }
                continue;
            }
            // ponytail: unknown system-message source — fall through to a user
            // record; context still resumes, byte-identity may not.
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
            // Wire "system" messages (hook outputs re-rendered per request) keep
            // their role so a resume replays them byte-identically.
            rec.insert("message".into(), json!({"role": role, "content": content}));
            if role == "user" && first_user_text.is_none() {
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

/// Claude Code injects the claudeMd context `<system-reminder>` into the
/// first user message at request time — it is never stored in the native
/// transcript. A resume re-injects it, so a fork that stored it would send
/// it twice. Drop those leading blocks; resume regenerates them.
const INJECTED_CONTEXT_MARKER: &str =
    "<system-reminder>\nAs you answer the user's questions, you can use the following context:";

// Per-attachment render formulas, verified against a real 2.1.210 session's
// stored attachments vs its wire capture. Attachments join with "\n\n" into
// one system message; the inverse split below must reproduce each stored
// attachment byte-for-byte or Claude's resume-time hook dedup fails and the
// resumed request grows a duplicate system message.
const AGENT_LISTING_HEADER: &str = "Available agent types for the Agent tool:\n";
const CONCURRENCY_NOTE: &str = "When you launch multiple agents for independent work, send them in a single message with multiple tool uses so they run concurrently.";
const SKILL_LISTING_HEADER: &str =
    "The following skills are available for use with the Skill tool:\n\n";

/// Invert a rendered system message into its constituent attachment payloads.
/// Returns None when the blob doesn't start with a recognized marker (unknown
/// source — caller falls back to a plain record).
fn parse_system_blob(text: &str) -> Option<Vec<Value>> {
    // marker positions: (byte offset of segment start, kind)
    let mut marks: Vec<(usize, &str)> = Vec::new();
    for (idx, _) in text.match_indices(" hook success: ") {
        // hookName runs back from idx to the preceding "\n\n" (or start)
        let name_start = text[..idx].rfind("\n\n").map(|p| p + 2).unwrap_or(0);
        let name = &text[name_start..idx];
        if !name.is_empty() && !name.contains('\n') && name.len() < 80 {
            marks.push((name_start, "hook"));
        }
    }
    for (idx, _) in text.match_indices(AGENT_LISTING_HEADER) {
        if idx == 0 || text[..idx].ends_with("\n\n") {
            marks.push((idx, "agents"));
        }
    }
    for (idx, _) in text.match_indices(SKILL_LISTING_HEADER) {
        if idx == 0 || text[..idx].ends_with("\n\n") {
            marks.push((idx, "skills"));
        }
    }
    marks.sort();
    marks.dedup();
    if marks.first().map(|m| m.0) != Some(0) {
        return None;
    }
    let mut out = Vec::new();
    for (i, &(start, kind)) in marks.iter().enumerate() {
        let end = marks
            .get(i + 1)
            .map(|m| m.0.saturating_sub(2)) // trim the "\n\n" joiner
            .unwrap_or(text.len());
        let seg = &text[start..end];
        match kind {
            "hook" => {
                let (name, content) = seg.split_once(" hook success: ")?;
                out.push(json!({
                    "type": "hook_success",
                    "hookName": name,
                    "toolUseID": uuid::Uuid::new_v4().to_string(),
                    "hookEvent": name.split(':').next().unwrap_or(name),
                    "content": content,
                }));
            }
            "agents" => {
                let body = seg.strip_prefix(AGENT_LISTING_HEADER)?;
                let (listing, note) = match body.rfind(&format!("\n\n{CONCURRENCY_NOTE}")) {
                    Some(p) => (&body[..p], true),
                    None => (body, false),
                };
                let lines: Vec<&str> = listing.split('\n').collect();
                let types: Vec<String> = lines
                    .iter()
                    .filter_map(|l| l.strip_prefix("- "))
                    .filter_map(|l| l.split(':').next())
                    .map(String::from)
                    .collect();
                out.push(json!({
                    "type": "agent_listing_delta",
                    "addedTypes": types,
                    "addedLines": lines,
                    "removedTypes": [],
                    "isInitial": true,
                    "showConcurrencyNote": note,
                }));
            }
            "skills" => {
                let content = seg.strip_prefix(SKILL_LISTING_HEADER)?;
                out.push(json!({"type": "skill_listing", "content": content}));
            }
            _ => unreachable!(),
        }
    }
    Some(out)
}

/// A one-element array of a bare text block is the request-time form of a
/// plain-string stored message; collapse it back.
fn collapse_single_text(content: Value) -> Value {
    if let Value::Array(a) = &content {
        if a.len() == 1 {
            let b = &a[0];
            if b.get("type").and_then(Value::as_str) == Some("text")
                && b.as_object().is_some_and(|o| o.len() == 2)
            {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    return Value::String(t.to_string());
                }
            }
        }
    }
    content
}

fn strip_injected_context(content: Value) -> Value {
    let Value::Array(blocks) = content else {
        return content;
    };
    Value::Array(
        blocks
            .into_iter()
            .filter(|b| {
                !(b.get("type").and_then(Value::as_str) == Some("text")
                    && b.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.starts_with(INJECTED_CONTEXT_MARKER)))
            })
            .collect(),
    )
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
