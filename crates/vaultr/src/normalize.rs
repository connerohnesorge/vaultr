//! Harness-agnostic transcript model, converted from reconstructed wire history.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    Image,
    ToolUse { name: String, input: Value },
    ToolResult { content: String },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

/// Convert reconstructed wire messages (Anthropic messages or Codex Responses
/// input items) into the normalized model. Drops system/developer scaffolding,
/// reasoning/thinking, opaque items, and `<system-reminder>` blocks in user text.
pub fn normalize(messages: &[Value]) -> Vec<Message> {
    let mut out = Vec::new();
    for m in messages {
        let Some(msg) = normalize_one(m) else {
            continue;
        };
        if !msg.blocks.is_empty() {
            out.push(msg);
        }
    }
    out
}

fn normalize_one(m: &Value) -> Option<Message> {
    let obj = m.as_object()?;
    let item_type = obj.get("type").and_then(Value::as_str);
    // Codex non-message items live at the top level of the input array.
    match item_type {
        Some("function_call") | Some("custom_tool_call") | Some("local_shell_call") => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(item_type.unwrap_or("tool"))
                .to_string();
            let raw = obj.get("arguments").or_else(|| obj.get("input"));
            let input = match raw {
                Some(Value::String(s)) => {
                    serde_json::from_str(s).unwrap_or(Value::String(s.clone()))
                }
                Some(v) => v.clone(),
                None => Value::Null,
            };
            return Some(Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse { name, input }],
            });
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            let content = text_of(obj.get("output").unwrap_or(&Value::Null));
            return Some(Message {
                role: Role::User,
                blocks: vec![Block::ToolResult { content }],
            });
        }
        Some("reasoning") => return None, // opaque/encrypted reasoning
        Some("message") | None => {}      // fall through to role-based handling
        Some(_) => return None,           // telemetry / UI / unknown item kinds
    }

    let role = match obj.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None, // system / developer / missing role
    };
    let content = obj.get("content")?;
    let mut blocks = Vec::new();
    match content {
        Value::String(s) => push_text(&mut blocks, s, &role),
        Value::Array(arr) => {
            for b in arr {
                if let Value::String(s) = b {
                    push_text(&mut blocks, s, &role);
                    continue;
                }
                let Some(bt) = b.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match bt {
                    "text" | "input_text" | "output_text" => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            push_text(&mut blocks, t, &role);
                        }
                    }
                    "image" | "input_image" => blocks.push(Block::Image),
                    "tool_use" => blocks.push(Block::ToolUse {
                        name: b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string(),
                        input: b.get("input").cloned().unwrap_or(Value::Null),
                    }),
                    "tool_result" => blocks.push(Block::ToolResult {
                        content: text_of(b.get("content").unwrap_or(&Value::Null)),
                    }),
                    // thinking / redacted_thinking / anything else: excluded
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Some(Message { role, blocks })
}

/// Push a text block, stripping `<system-reminder>` scaffolding from user text.
fn push_text(blocks: &mut Vec<Block>, text: &str, role: &Role) {
    let cleaned = if *role == Role::User {
        strip_system_reminders(text)
    } else {
        text.to_string()
    };
    let trimmed = cleaned.trim();
    if !trimmed.is_empty() {
        blocks.push(Block::Text(trimmed.to_string()));
    }
}

/// Remove `<system-reminder>...</system-reminder>` spans (like recon.mjs).
pub fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => return out, // unterminated: drop the tail
        }
    }
    out.push_str(rest);
    out
}

/// Flatten arbitrary tool-output content into plain text.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| match b {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("text").and_then(Value::as_str).map(String::from),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
