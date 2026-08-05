//! Markdown rendering of a normalized transcript. Tool calls are one-line
//! markers; tool results, reasoning, and scaffolding are omitted.

use crate::normalize::{Block, Message, Role};

const TOOL_INPUT_MAX: usize = 120;

pub fn markdown(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let mut body = String::new();
        for b in &m.blocks {
            match b {
                Block::Text(t) => {
                    body.push_str(t);
                    body.push_str("\n\n");
                }
                Block::Image => body.push_str("*[image]*\n\n"),
                Block::ToolUse { name, input, .. } => {
                    let compact = serde_json::to_string(input).unwrap_or_default();
                    body.push_str(&format!(
                        "> `{}` {}\n\n",
                        name,
                        truncate(&compact, TOOL_INPUT_MAX)
                    ));
                }
                Block::ToolResult { .. } => {} // never rendered
            }
        }
        if body.trim().is_empty() {
            continue; // e.g. a user turn that was only tool results
        }
        let heading = match m.role {
            Role::User => "## User",
            Role::Assistant => "## Assistant",
        };
        out.push_str(heading);
        out.push_str("\n\n");
        out.push_str(body.trim_end());
        out.push_str("\n\n");
    }
    out.trim_end().to_string() + "\n"
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}
