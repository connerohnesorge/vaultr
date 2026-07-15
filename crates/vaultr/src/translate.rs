//! Cross-harness tool translation: best-effort Codex<->Claude tool-name
//! mapping plus conversion of the normalized transcript model into native
//! wire histories. Unmapped tools degrade to readable plain text — a fork
//! must never fail because of a tool it cannot translate.

use crate::normalize::{Block, Message, Role};
use serde_json::{json, Value};

/// Codex tool name -> Claude tool name.
pub fn codex_to_claude(name: &str) -> Option<&'static str> {
    Some(match name {
        "shell" | "exec_command" | "local_shell" | "local_shell_call" | "container.exec" => "Bash",
        "apply_patch" => "Edit",
        "web_search" | "web.search" => "WebSearch",
        "update_plan" => "TodoWrite",
        "view_image" | "read_file" => "Read",
        "list_dir" | "list_files" => "Glob",
        "grep" | "search" => "Grep",
        _ => return None,
    })
}

/// Claude tool name -> Codex tool name.
pub fn claude_to_codex(name: &str) -> Option<&'static str> {
    Some(match name {
        "Bash" => "shell",
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "apply_patch",
        "WebSearch" => "web_search",
        "TodoWrite" => "update_plan",
        "Read" => "view_image",
        "Glob" => "list_dir",
        "Grep" => "grep",
        _ => return None,
    })
}

/// Render an unmapped tool call as readable plain text.
fn tool_use_text(name: &str, input: &Value) -> String {
    let args = serde_json::to_string(input).unwrap_or_default();
    format!("[tool call: {name}({args})]")
}

fn tool_result_text(content: &str) -> String {
    format!("[tool result]\n{content}")
}

/// Pairing state: for each ToolUse emitted, remember Some(id) when it became
/// a native tool call (so its result must reference that id), or None when it
/// degraded to text (so its result degrades to text too).
type PendingIds = std::collections::VecDeque<Option<String>>;

/// Convert normalized messages into Anthropic wire messages (for a Claude fork).
pub fn to_anthropic(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending: PendingIds = Default::default();
    for m in messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut blocks: Vec<Value> = Vec::new();
        for b in &m.blocks {
            match b {
                Block::Text(t) => blocks.push(json!({"type": "text", "text": t})),
                Block::Image => blocks.push(json!({"type": "text", "text": "[image]"})),
                Block::ToolUse { name, input } => match codex_to_claude(name) {
                    Some(mapped) => {
                        let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                        blocks.push(json!({
                            "type": "tool_use", "id": id, "name": mapped,
                            "input": adapt_input_to_claude(mapped, input),
                        }));
                        pending.push_back(Some(id));
                    }
                    None => {
                        blocks.push(json!({"type": "text", "text": tool_use_text(name, input)}));
                        pending.push_back(None);
                    }
                },
                Block::ToolResult { content } => match pending.pop_front().flatten() {
                    Some(id) => blocks.push(json!({
                        "type": "tool_result", "tool_use_id": id, "content": content,
                    })),
                    None => blocks.push(json!({"type": "text", "text": tool_result_text(content)})),
                },
            }
        }
        if !blocks.is_empty() {
            out.push(json!({"role": role, "content": blocks}));
        }
    }
    out
}

/// Convert normalized messages into Codex Responses input items (for a Codex fork).
pub fn to_codex(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending: PendingIds = Default::default();
    for m in messages {
        let mut texts: Vec<Value> = Vec::new();
        let (role, text_type) = match m.role {
            Role::User => ("user", "input_text"),
            Role::Assistant => ("assistant", "output_text"),
        };
        let flush = |texts: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !texts.is_empty() {
                out.push(json!({
                    "type": "message", "role": role,
                    "content": std::mem::take(texts),
                }));
            }
        };
        for b in &m.blocks {
            match b {
                Block::Text(t) => texts.push(json!({"type": text_type, "text": t})),
                Block::Image => texts.push(json!({"type": text_type, "text": "[image]"})),
                Block::ToolUse { name, input } => match claude_to_codex(name) {
                    Some(mapped) => {
                        flush(&mut texts, &mut out);
                        let call_id = format!("call_{}", uuid::Uuid::new_v4().simple());
                        out.push(json!({
                            "type": "function_call",
                            "name": mapped,
                            "arguments": serde_json::to_string(input).unwrap_or_default(),
                            "call_id": call_id,
                        }));
                        pending.push_back(Some(call_id));
                    }
                    None => {
                        texts.push(json!({"type": text_type, "text": tool_use_text(name, input)}));
                        pending.push_back(None);
                    }
                },
                Block::ToolResult { content } => match pending.pop_front().flatten() {
                    Some(call_id) => {
                        flush(&mut texts, &mut out);
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": [{"type": "input_text", "text": content}],
                        }));
                    }
                    None => {
                        texts.push(json!({"type": text_type, "text": tool_result_text(content)}))
                    }
                },
            }
        }
        flush(&mut texts, &mut out);
    }
    out
}

/// Best-effort input adaptation for a Codex tool call mapped onto a Claude tool.
fn adapt_input_to_claude(mapped: &str, input: &Value) -> Value {
    if mapped == "Bash" {
        // Codex shell: {"command": ["bash","-lc","..."]} or {"cmd": "..."}.
        let cmd = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .cloned()
            .unwrap_or(Value::Null);
        let text = match &cmd {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => a.last().and_then(Value::as_str).map(String::from),
            _ => None,
        };
        if let Some(t) = text {
            return json!({"command": t});
        }
    }
    input.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                blocks: vec![Block::Text("run it".into())],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    name: "shell".into(),
                    input: json!({"command": ["bash", "-lc", "ls"]}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    content: "a.txt".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    name: "weird_tool".into(),
                    input: json!({"x": 1}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    content: "ok".into(),
                }],
            },
        ]
    }

    #[test]
    fn codex_to_claude_maps_and_pairs() {
        let out = to_anthropic(&msgs());
        let tu = &out[1]["content"][0];
        assert_eq!(tu["type"], "tool_use");
        assert_eq!(tu["name"], "Bash");
        assert_eq!(tu["input"]["command"], "ls");
        let tr = &out[2]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], tu["id"]);
    }

    #[test]
    fn unmapped_tool_degrades_to_text() {
        let out = to_anthropic(&msgs());
        let tu = &out[3]["content"][0];
        assert_eq!(tu["type"], "text");
        assert!(tu["text"].as_str().unwrap().contains("weird_tool"));
        let tr = &out[4]["content"][0];
        assert_eq!(tr["type"], "text");
        assert!(tr["text"].as_str().unwrap().contains("[tool result]"));
    }

    #[test]
    fn claude_to_codex_maps_bash() {
        let m = vec![
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    content: "a.txt".into(),
                }],
            },
        ];
        let out = to_codex(&m);
        assert_eq!(out[0]["type"], "function_call");
        assert_eq!(out[0]["name"], "shell");
        assert_eq!(out[1]["type"], "function_call_output");
        assert_eq!(out[1]["call_id"], out[0]["call_id"]);
        assert_eq!(out[1]["output"][0]["type"], "input_text");
    }

    #[test]
    fn unmapped_claude_tool_degrades_in_codex() {
        let m = vec![
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    name: "Artifact".into(),
                    input: json!({"file_path": "x.html"}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    content: "done".into(),
                }],
            },
        ];
        let out = to_codex(&m);
        assert_eq!(out[0]["type"], "message");
        assert!(out[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Artifact"));
        assert!(out[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[tool result]"));
    }
}
