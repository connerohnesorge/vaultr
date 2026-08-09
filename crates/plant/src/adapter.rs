//! Harness adapters — mirrors ADAPTERS in wireproxy.ts.

use crate::domain::Harness;
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

pub struct Adapter {
    pub harness: Harness,
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
            harness: Harness::ClaudeCode,
            port: env_port("VAULTR_ANTHROPIC_PORT", 18923),
            upstream: env_or("VAULTR_ANTHROPIC_UPSTREAM", "https://api.anthropic.com"),
            history_key: "messages",
            big_fields: &["tools", "system"],
            terminal_event: "message_stop",
        },
        Adapter {
            harness: Harness::Codex,
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
    /// Host of `upstream`, but only when the base carries no path prefix.
    ///
    /// This is the sole test for "may Plant terminate TLS for this CONNECT".
    /// A base with a path (Codex's `.../backend-api/codex`) is excluded: an
    /// intercepted origin-form request would be re-appended to that prefix and
    /// reach the wrong URL, so such hosts get a blind tunnel instead. Derived
    /// from `upstream` rather than hardcoded so `VAULTR_ANTHROPIC_UPSTREAM`
    /// keeps working — including the self-test's plain-HTTP fake upstream.
    ///
    /// The port is stripped and never matched on: a client only issues CONNECT
    /// for an `https://` URL, so TLS always follows the 200 regardless of port.
    pub fn interceptable_host(&self) -> Option<&str> {
        let rest = self
            .upstream
            .strip_prefix("https://")
            .or_else(|| self.upstream.strip_prefix("http://"))?
            .trim_end_matches('/');
        if rest.contains('/') || rest.contains('@') {
            return None;
        }
        let host = rest.split(':').next().unwrap_or_default();
        (!host.is_empty()).then_some(host)
    }

    pub fn captures(&self, method: &str, path: &str) -> bool {
        match self.harness {
            // Exact match, not a prefix: /v1/messages/count_tokens carries no
            // metadata.user_id, so a prefix match parsed its (full-history) body
            // only to fail identity extraction and drop it — spamming
            // "capture failed: no session identity" and burning a DOM_GATE permit
            // that real turns contend for. count_tokens is a side-query, not a turn.
            Harness::ClaudeCode => method == "POST" && path == "/v1/messages",
            Harness::Codex => method == "POST" && path.ends_with("/responses"),
        }
    }

    /// Pi's Codex provider appends `/codex/responses` to a bare proxy base.
    /// Plant's upstream already ends in `/codex`, so strip that exact duplicate
    /// only for transport while preserving the observed path in the Envelope.
    pub fn upstream_path<'a>(&self, path: &'a str) -> &'a str {
        if self.harness == Harness::Codex && path == "/codex/responses" {
            "/responses"
        } else {
            path
        }
    }

    /// A response is complete only when the transport ended cleanly and a
    /// parsed SSE event has the exact terminal type for this harness.
    pub fn response_complete(&self, events: &[Value], transport_complete: bool) -> bool {
        transport_complete
            && events
                .iter()
                .any(|event| event.get("type").and_then(Value::as_str) == Some(self.terminal_event))
    }

    pub fn identity(&self, headers: &hyper::HeaderMap, body: &Value) -> Identity {
        match self.harness {
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
                // Two clients speak this Responses dialect and spell the identity headers
                // differently: the codex CLI sends `session-id`/`thread-id`, prime-agent
                // sends the same values as `session_id`/`x-client-request-id`. Accept the
                // underscore spelling too, or every prime-agent turn is dropped with
                // "codex request has no session identity" and never reaches the vault.
                let pick = |hdr: &str, keys: [&str; 1]| -> Option<String> {
                    header_str(headers, hdr)
                        .filter(|s| is_uuid(s))
                        .or_else(|| header_str(headers, keys[0]).filter(|s| is_uuid(s)))
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
        match self.harness {
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
                // Responses-API usage: input_tokens is the TOTAL input; cached_tokens
                // and cache_write_tokens are subsets of it (total = input + output).
                // Subtract them so buckets don't overlap, matching Claude semantics.
                let input_total = count(usage.get("input_tokens"));
                let cached = count(usage.pointer("/input_tokens_details/cached_tokens"));
                let cache_write = count(usage.pointer("/input_tokens_details/cache_write_tokens"));
                TokenUsage {
                    input: input_total
                        .saturating_sub(cached)
                        .saturating_sub(cache_write),
                    output: count(usage.get("output_tokens")),
                    cache_read: cached,
                    cache_creation: cache_write,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Adapter {
        adapters().remove(0)
    }
    fn codex() -> Adapter {
        adapters().remove(1)
    }

    #[test]
    fn captures_streaming_messages_but_not_count_tokens() {
        let c = claude();
        // the streaming turn endpoint — captured (query is stripped before this call)
        assert!(c.captures("POST", "/v1/messages"));
        // count_tokens is a side-query with no session identity — must NOT be captured
        assert!(!c.captures("POST", "/v1/messages/count_tokens"));
        // batches likewise are not interactive turns
        assert!(!c.captures("POST", "/v1/messages/batches"));
        // wrong method
        assert!(!c.captures("GET", "/v1/messages"));

        let x = codex();
        assert!(x.captures("POST", "/backend-api/codex/responses"));
        assert!(!x.captures("POST", "/backend-api/codex/responses/count_tokens"));
        assert_eq!(x.upstream_path("/codex/responses"), "/responses");
        assert_eq!(x.upstream_path("/responses"), "/responses");
        assert_eq!(
            x.upstream_path("/prefix/codex/responses"),
            "/prefix/codex/responses"
        );
    }

    #[test]
    fn codex_identity_accepts_both_client_header_spellings() {
        let codex = codex();
        let sid = "019fe783-95e3-71fa-8d03-44b947a6c8f6";
        let body = serde_json::json!({});

        // codex CLI spelling
        let mut hyphen = hyper::HeaderMap::new();
        hyphen.insert("session-id", sid.parse().unwrap());
        assert_eq!(
            codex.identity(&hyphen, &body).session_id.as_deref(),
            Some(sid)
        );

        // prime-agent spelling — without this the turn is dropped for want of identity
        let mut underscore = hyper::HeaderMap::new();
        underscore.insert("session_id", sid.parse().unwrap());
        assert_eq!(
            codex.identity(&underscore, &body).session_id.as_deref(),
            Some(sid)
        );

        // a non-uuid in either spelling is still not an identity
        let mut junk = hyper::HeaderMap::new();
        junk.insert("session_id", "not-a-uuid".parse().unwrap());
        assert_eq!(codex.identity(&junk, &body).session_id, None);

        // neither spelling present stays absent rather than inventing one
        assert_eq!(
            codex.identity(&hyper::HeaderMap::new(), &body).session_id,
            None
        );
    }

    #[test]
    fn completion_requires_an_exact_top_level_terminal_event() {
        let claude = claude();
        let events = vaultr::recon::parse_sse(
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"message_stop"}}"#,
        );
        assert!(!claude.response_complete(&events, true));
        let events = vaultr::recon::parse_sse(r#"data: {"type":"message_stop"}"#);
        assert!(claude.response_complete(&events, true));

        let codex = codex();
        let events = vaultr::recon::parse_sse(
            r#"data: {"type":"response.output_text.delta","delta":"response.completed"}"#,
        );
        assert!(!codex.response_complete(&events, true));
        let events = vaultr::recon::parse_sse(r#"data: {"type":"response.completed"}"#);
        assert!(codex.response_complete(&events, true));
    }

    #[test]
    fn torn_or_disconnected_transport_cannot_complete() {
        let claude = claude();
        let events = vaultr::recon::parse_sse(r#"data: {"type":"message_stop"}"#);
        for case in ["torn stream", "client disconnect"] {
            assert!(!claude.response_complete(&events, false), "{case}");
        }

        let codex = codex();
        let events = vaultr::recon::parse_sse(r#"data: {"type":"response.completed"}"#);
        for case in ["torn stream", "client disconnect"] {
            assert!(!codex.response_complete(&events, false), "{case}");
        }
    }

    #[test]
    fn codex_cached_tokens_are_not_double_counted() {
        // real capture values: cached_tokens is a subset of input_tokens
        let events = vaultr::recon::parse_sse(
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":25296,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":24320},"output_tokens":18,"total_tokens":25314}}}"#,
        );
        let u = codex().usage(&events);
        assert_eq!(u.input, 976); // 25296 - 24320 - 0
        assert_eq!(u.cache_read, 24320);
        assert_eq!(u.cache_creation, 0);
        assert_eq!(u.output, 18);
        // non-overlapping: buckets reconstruct the reported total input side
        assert_eq!(u.input + u.cache_read + u.cache_creation, 25296);

        // cache_write is captured (not dropped) and also excluded from input
        let events = vaultr::recon::parse_sse(
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1000,"input_tokens_details":{"cache_write_tokens":300,"cached_tokens":200},"output_tokens":5}}}"#,
        );
        let u = codex().usage(&events);
        assert_eq!(u.input, 500);
        assert_eq!(u.cache_read, 200);
        assert_eq!(u.cache_creation, 300);
        assert_eq!(u.input + u.cache_read + u.cache_creation, 1000);
    }
}
