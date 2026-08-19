//! Native Pi session writer: emits a resumable version 3 JSONL session under
//! Pi's cwd-encoded session directory.

use crate::normalize::{Block, Message, Role};
use crate::translate::{tool_result_text, tool_use_text, valid_correlations, ToolKind};
use crate::writeio::write_atomic_0600;
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const PI_SESSION_VERSION: u8 = 3;

/// Encode an absolute cwd the same way Pi encodes its default session directory.
pub fn encode_session_dir(cwd: &str) -> String {
    let encoded: String = cwd
        .trim_start_matches(['/', '\\'])
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{encoded}--")
}

/// Write a Pi session with a fresh UUIDv7. Returns (session id, path).
pub fn write(
    session_root: &Path,
    cwd: &str,
    provider: &str,
    api: &str,
    model: Option<&str>,
    messages: &[Message],
) -> Result<(String, PathBuf)> {
    let id = uuid::Uuid::now_v7().to_string();
    let path = write_with_id(
        session_root,
        cwd,
        provider,
        api,
        model,
        messages,
        &id,
        Utc::now(),
    )?;
    Ok((id, path))
}

/// Write a Pi session under an explicit id and timestamp (exposed for tests).
#[allow(clippy::too_many_arguments)]
pub fn write_with_id(
    session_root: &Path,
    cwd: &str,
    provider: &str,
    api: &str,
    model: Option<&str>,
    messages: &[Message],
    session_id: &str,
    started_at: DateTime<Utc>,
) -> Result<PathBuf> {
    if messages.is_empty() {
        bail!("nothing to fork: reconstructed history is empty");
    }

    let dir = session_root.join(encode_session_dir(cwd));
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!(
        "{}_{session_id}.jsonl",
        started_at.format("%Y-%m-%dT%H-%M-%S-%3fZ")
    ));

    let mut lines = vec![serde_json::to_string(&json!({
        "type": "session",
        "version": PI_SESSION_VERSION,
        "id": session_id,
        "timestamp": iso(started_at),
        "cwd": cwd,
    }))?];
    let mut ids = HashSet::new();
    let mut parent: Option<String> = None;
    let mut ts = started_at;

    if let Some(model) = model {
        append_entry(
            &mut lines,
            &mut ids,
            &mut parent,
            &mut ts,
            json!({"type": "model_change", "provider": provider, "modelId": model}),
        )?;
    }

    let valid = valid_correlations(messages);
    let mut calls: HashMap<&str, (String, String)> = HashMap::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                let mut content = Vec::new();
                for block in &message.blocks {
                    match block {
                        Block::Text(text) => content.push(json!({"type": "text", "text": text})),
                        Block::Image => content.push(json!({"type": "text", "text": "[image]"})),
                        Block::ToolUse {
                            name,
                            input,
                            correlation_id,
                        } => {
                            let mapped = correlation_id
                                .as_deref()
                                .filter(|id| valid.contains(*id))
                                .and_then(|id| {
                                    pi_tool(name, input).map(|(tool, arguments)| {
                                        let call_id =
                                            format!("call_{}", uuid::Uuid::new_v4().simple());
                                        calls.insert(id, (call_id.clone(), tool.to_string()));
                                        (call_id, tool, arguments)
                                    })
                                });
                            if let Some((id, name, arguments)) = mapped {
                                content.push(json!({
                                    "type": "toolCall",
                                    "id": id,
                                    "name": name,
                                    "arguments": arguments,
                                }));
                            } else {
                                content.push(json!({
                                    "type": "text",
                                    "text": tool_use_text(name, input),
                                }));
                            }
                        }
                        Block::ToolResult { content: text, .. } => content.push(json!({
                            "type": "text",
                            "text": tool_result_text(text),
                        })),
                    }
                }
                if !content.is_empty() {
                    let stop_reason = if content
                        .iter()
                        .any(|block| block["type"].as_str() == Some("toolCall"))
                    {
                        "toolUse"
                    } else {
                        "stop"
                    };
                    let message = json!({
                        "role": "assistant",
                        "content": content,
                        "api": api,
                        "provider": provider,
                        "model": model.unwrap_or("unknown"),
                        "usage": zero_usage(),
                        "stopReason": stop_reason,
                        "timestamp": millis(ts + Duration::milliseconds(1)),
                    });
                    append_message(&mut lines, &mut ids, &mut parent, &mut ts, message)?;
                }
            }
            Role::User => {
                let mut text_blocks = Vec::new();
                for block in &message.blocks {
                    match block {
                        Block::Text(text) => {
                            text_blocks.push(json!({"type": "text", "text": text}))
                        }
                        Block::Image => {
                            text_blocks.push(json!({"type": "text", "text": "[image]"}))
                        }
                        Block::ToolResult {
                            content,
                            correlation_id,
                        } => {
                            flush_user_text(
                                &mut lines,
                                &mut ids,
                                &mut parent,
                                &mut ts,
                                &mut text_blocks,
                            )?;
                            if let Some((call_id, tool_name)) =
                                correlation_id.as_deref().and_then(|id| calls.get(id))
                            {
                                let message_ts = millis(ts + Duration::milliseconds(1));
                                append_message(
                                    &mut lines,
                                    &mut ids,
                                    &mut parent,
                                    &mut ts,
                                    json!({
                                        "role": "toolResult",
                                        "toolCallId": call_id,
                                        "toolName": tool_name,
                                        "content": [{"type": "text", "text": content}],
                                        "isError": false,
                                        "timestamp": message_ts,
                                    }),
                                )?;
                            } else {
                                text_blocks.push(json!({
                                    "type": "text",
                                    "text": tool_result_text(content),
                                }));
                            }
                        }
                        Block::ToolUse { name, input, .. } => text_blocks.push(json!({
                            "type": "text",
                            "text": tool_use_text(name, input),
                        })),
                    }
                }
                flush_user_text(&mut lines, &mut ids, &mut parent, &mut ts, &mut text_blocks)?;
            }
        }
    }

    if parent.is_none() {
        bail!("nothing to fork: normalized history is empty");
    }
    write_atomic_0600(&dest, &lines)?;
    Ok(dest)
}

fn flush_user_text(
    lines: &mut Vec<String>,
    ids: &mut HashSet<String>,
    parent: &mut Option<String>,
    ts: &mut DateTime<Utc>,
    content: &mut Vec<Value>,
) -> Result<()> {
    if content.is_empty() {
        return Ok(());
    }
    let message = json!({
        "role": "user",
        "content": std::mem::take(content),
        "timestamp": millis(*ts + Duration::milliseconds(1)),
    });
    append_message(lines, ids, parent, ts, message)
}

fn append_message(
    lines: &mut Vec<String>,
    ids: &mut HashSet<String>,
    parent: &mut Option<String>,
    ts: &mut DateTime<Utc>,
    message: Value,
) -> Result<()> {
    append_entry(
        lines,
        ids,
        parent,
        ts,
        json!({"type": "message", "message": message}),
    )
}

fn append_entry(
    lines: &mut Vec<String>,
    ids: &mut HashSet<String>,
    parent: &mut Option<String>,
    ts: &mut DateTime<Utc>,
    value: Value,
) -> Result<()> {
    *ts += Duration::milliseconds(1);
    let id = entry_id(ids);
    let mut entry = value.as_object().cloned().unwrap_or_default();
    entry.insert("id".into(), json!(id));
    entry.insert(
        "parentId".into(),
        parent.clone().map_or(Value::Null, Value::String),
    );
    entry.insert("timestamp".into(), json!(iso(*ts)));
    lines.push(serde_json::to_string(&Value::Object(entry))?);
    *parent = Some(id);
    Ok(())
}

fn entry_id(ids: &mut HashSet<String>) -> String {
    loop {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if ids.insert(id.clone()) {
            return id;
        }
    }
}

fn pi_tool<'a>(name: &'a str, input: &Value) -> Option<(&'a str, Value)> {
    let mapped = ToolKind::from_codex(name)
        .or_else(|| ToolKind::from_claude(name))
        .and_then(ToolKind::pi_name)?;
    let mut arguments = input.as_object()?.clone();
    rename_key(&mut arguments, "file_path", "path");
    rename_key(&mut arguments, "old_string", "oldText");
    rename_key(&mut arguments, "new_string", "newText");
    if mapped == "bash" {
        if let Some(Value::Array(command)) = arguments.get("command") {
            let command = command.last()?.as_str()?.to_string();
            arguments.insert("command".into(), Value::String(command));
        }
        rename_key(&mut arguments, "cmd", "command");
    }
    Some((mapped, Value::Object(arguments)))
}

fn rename_key(map: &mut Map<String, Value>, old: &str, new: &str) {
    if let Some(value) = map.remove(old) {
        map.entry(new.to_string()).or_insert(value);
    }
}

fn zero_usage() -> Value {
    json!({
        "input": 0,
        "output": 0,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 0,
        "cost": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "total": 0,
        },
    })
}

fn iso(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn millis(timestamp: DateTime<Utc>) -> i64 {
    timestamp.timestamp_millis()
}
