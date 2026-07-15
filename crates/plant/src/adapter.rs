//! Harness adapters — mirrors ADAPTERS in wireproxy.ts.

use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct Identity {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

pub struct Adapter {
    pub harness: &'static str,
    pub kind: Harness,
    pub port: u16,
    pub upstream: String,
    pub history_key: &'static str,
    pub big_fields: &'static [&'static str],
    pub terminal_event: &'static str,
}

fn env_port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_or(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

pub fn adapters() -> Vec<Adapter> {
    vec![
        Adapter {
            harness: "claude-code",
            kind: Harness::ClaudeCode,
            port: env_port("VAULTR_ANTHROPIC_PORT", 18923),
            upstream: env_or("VAULTR_ANTHROPIC_UPSTREAM", "https://api.anthropic.com"),
            history_key: "messages",
            big_fields: &["tools", "system"],
            terminal_event: "message_stop",
        },
        Adapter {
            harness: "codex",
            kind: Harness::Codex,
            port: env_port("VAULTR_CODEX_PORT", 18924),
            upstream: env_or(
                "VAULTR_CODEX_UPSTREAM",
                "https://chatgpt.com/backend-api/codex",
            ),
            history_key: "input",
            big_fields: &["tools", "instructions"],
            terminal_event: "response.completed",
        },
    ]
}

impl Adapter {
    pub fn captures(&self, method: &str, path: &str) -> bool {
        match self.kind {
            Harness::ClaudeCode => method == "POST" && path.starts_with("/v1/messages"),
            Harness::Codex => method == "POST" && path.ends_with("/responses"),
        }
    }

    pub fn identity(&self, headers: &hyper::HeaderMap, body: &Value) -> Identity {
        match self.kind {
            Harness::ClaudeCode => {
                let sid = body
                    .pointer("/metadata/user_id")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| {
                        v.get("session_id")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .filter(|s| is_uuid(s));
                Identity {
                    session_id: sid,
                    ..Default::default()
                }
            }
            Harness::Codex => {
                let client = object(
                    body.get("client_metadata")
                        .or_else(|| body.get("metadata"))
                        .cloned(),
                );
                let turn = object(
                    header_str(headers, "x-codex-turn-metadata")
                        .map(|s| Value::String(s.to_string()))
                        .or_else(|| client.get("x-codex-turn-metadata").cloned()),
                );
                let pick = |hdr: &str, keys: [&str; 1]| -> Option<String> {
                    header_str(headers, hdr)
                        .filter(|s| is_uuid(s))
                        .map(String::from)
                        .or_else(|| id_field(&turn, keys[0]))
                        .or_else(|| id_field(&client, keys[0]))
                };
                Identity {
                    session_id: pick("session-id", ["session_id"]),
                    thread_id: pick("thread-id", ["thread_id"]),
                    turn_id: id_field(&turn, "turn_id").or_else(|| id_field(&client, "turn_id")),
                }
            }
        }
    }

    pub fn usage(&self, events: &[Value]) -> TokenUsage {
        match self.kind {
            Harness::ClaudeCode => {
                let start = events
                    .iter()
                    .find(|e| e.get("type").and_then(Value::as_str) == Some("message_start"))
                    .and_then(|e| e.pointer("/message/usage"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let delta = events
                    .iter()
                    .rev()
                    .find(|e| e.get("type").and_then(Value::as_str) == Some("message_delta"))
                    .and_then(|e| e.get("usage"))
                    .cloned()
                    .unwrap_or(Value::Null);
                TokenUsage {
                    input: count(start.get("input_tokens")),
                    output: count(
                        delta
                            .get("output_tokens")
                            .or_else(|| start.get("output_tokens")),
                    ),
                    cache_read: count(start.get("cache_read_input_tokens")),
                    cache_creation: count(start.get("cache_creation_input_tokens")),
                }
            }
            Harness::Codex => {
                let usage = events
                    .iter()
                    .rev()
                    .find(|e| e.get("type").and_then(Value::as_str) == Some("response.completed"))
                    .and_then(|e| e.pointer("/response/usage"))
                    .cloned()
                    .unwrap_or(Value::Null);
                TokenUsage {
                    input: count(usage.get("input_tokens")),
                    output: count(usage.get("output_tokens")),
                    cache_read: count(usage.pointer("/input_tokens_details/cached_tokens")),
                    cache_creation: 0,
                }
            }
        }
    }
}

fn header_str<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// object(): value if object, JSON-parse if string, else {}
fn object(value: Option<Value>) -> Value {
    match value {
        Some(v @ Value::Object(_)) => v,
        Some(Value::String(s)) => {
            serde_json::from_str(&s).unwrap_or(Value::Object(Default::default()))
        }
        _ => Value::Object(Default::default()),
    }
}

fn id_field(obj: &Value, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| is_uuid(s))
        .map(String::from)
}

pub fn is_uuid(s: &str) -> bool {
    // UUID regex from wireproxy.ts:32 — version 1-8, variant 89ab
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            14 => {
                if !(b'1'..=b'8').contains(&c.to_ascii_lowercase()) {
                    return false;
                }
            }
            19 => {
                if !matches!(c.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b') {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

fn count(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
            .unwrap_or(0),
        Some(Value::String(s)) => s
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn parse_sse(sse: &str) -> Vec<Value> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}
