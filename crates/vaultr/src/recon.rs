//! Streaming reconstruction of the final message history from a turns.jsonl
//! (raw or zstd) capture. Memory is bounded by the largest single envelope
//! plus the final history — the archive is never loaded whole.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// Result of reconstructing a capture.
pub struct Recon {
    /// History key seen on the wire: "messages" (Anthropic) or "input" (Codex).
    pub key: String,
    /// Message count from history deltas alone (matches recon.mjs `count`).
    pub history_len: usize,
    /// Final history, with the trailing completed response (if any) appended.
    pub messages: Vec<Value>,
    /// Number of trailing assistant items appended from the final response.
    pub trailing_appended: usize,
    /// Envelopes parsed.
    pub envelopes: usize,
}

/// Reconstruct from a capture file path (`.zst` handled transparently).
pub fn reconstruct(path: &Path) -> Result<Recon> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        let dec = zstd::Decoder::new(file).context("zstd decoder")?;
        reconstruct_reader(dec)
    } else {
        reconstruct_reader(file)
    }
}

/// Streaming core: line-by-line over any reader.
pub fn reconstruct_reader<R: Read>(reader: R) -> Result<Recon> {
    let mut lines = BufReader::new(reader);
    let mut msgs: Vec<Value> = Vec::new();
    let mut hash_dict: HashMap<String, Value> = HashMap::new();
    let mut key = String::from("messages");
    let mut harness = String::new();
    let mut trailing: Vec<Value> = Vec::new();
    let mut envelopes = 0usize;
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = lines.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        // A truncated live-tail final line fails to parse — ignore it (and any
        // other malformed line), snapshotting through the last complete envelope.
        let Ok(env) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        envelopes += 1;
        if let Some(h) = env.get("harness").and_then(Value::as_str) {
            harness = h.to_string();
        }
        // The response of every envelope *before* the last is reflected in the
        // next request's history delta; only the final envelope's completed
        // response needs appending. Track it per-envelope, keeping only the last.
        trailing = extract_response_output(&env, &harness);
        // Codex stamps each replayed item with the turn it belongs to; the
        // request-side items of this turn carry it already (baked into the
        // wire), but the response-side items we append here don't — add it so
        // a fork's resume replays them byte-identically to a native resume.
        if harness == "codex" {
            if let Some(turn_id) = env.get("turn_id").and_then(Value::as_str) {
                for item in &mut trailing {
                    if let Some(o) = item.as_object_mut() {
                        o.insert(
                            "internal_chat_message_metadata_passthrough".into(),
                            serde_json::json!({"turn_id": turn_id}),
                        );
                    }
                }
            }
        }

        let Some(h) = env.pointer("/request/body_delta/history") else {
            continue;
        };
        if let Some(k) = h.get("key").and_then(Value::as_str) {
            key = k.to_string();
        }
        apply_delta(h, &mut msgs, &mut hash_dict);
    }

    let history_len = msgs.len();
    let trailing_appended = trailing.len();
    msgs.extend(trailing);
    Ok(Recon {
        key,
        history_len,
        messages: msgs,
        trailing_appended,
        envelopes,
    })
}

/// Apply one history delta (append or content-addressed form), mirroring recon.mjs.
fn apply_delta(h: &Value, msgs: &mut Vec<Value>, hash_dict: &mut HashMap<String, Value>) {
    let append = h.get("append").and_then(Value::as_array);
    let prefix = h.get("prefix_length").and_then(Value::as_u64);
    if let (Some(append), Some(prefix)) = (append, prefix) {
        msgs.truncate(prefix as usize);
        msgs.extend(append.iter().cloned());
    } else if let Some(order) = h.get("order").and_then(Value::as_array) {
        if let Some(new) = h.get("new").and_then(Value::as_object) {
            for (k, v) in new {
                hash_dict.insert(k.clone(), v.clone());
            }
        }
        *msgs = order
            .iter()
            .filter_map(|x| x.as_str().and_then(|k| hash_dict.get(k)).cloned())
            .collect();
    } else if let Some(append) = append {
        msgs.extend(append.iter().cloned());
    }
}

/// Extract the assistant output carried by an envelope's completed response.
/// v1 envelopes carry `response.sse` (raw SSE text); v2 carry `response.events`
/// (parsed event array). Returns wire-shaped items ready to append to history:
/// Anthropic → one assistant message; Codex → the Responses output items.
fn extract_response_output(env: &Value, harness: &str) -> Vec<Value> {
    let Some(resp) = env.get("response") else {
        return vec![];
    };
    if resp.get("complete").and_then(Value::as_bool) != Some(true) {
        return vec![];
    }
    let events: Vec<Value> = if let Some(evs) = resp.get("events").and_then(Value::as_array) {
        evs.clone()
    } else if let Some(sse) = resp.get("sse").and_then(Value::as_str) {
        parse_sse(sse)
    } else {
        return vec![];
    };
    if harness == "codex"
        || env
            .pointer("/request/body_delta/history/key")
            .and_then(Value::as_str)
            == Some("input")
    {
        codex_output(&events)
    } else {
        anthropic_output(&events)
    }
}

/// Codex Responses: prefer explicit output_item.done items; fall back to
/// response.completed's `response.output` (often empty with store:false).
fn codex_output(events: &[Value]) -> Vec<Value> {
    let done: Vec<Value> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("response.output_item.done"))
        .filter_map(|e| e.get("item").cloned())
        .collect();
    if !done.is_empty() {
        return done;
    }
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("response.completed"))
        .and_then(|e| e.pointer("/response/output"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Anthropic SSE accumulation: message_start seeds the message; content blocks
/// grow via content_block_start/_delta. Returns one assistant message, or none
/// if the stream never terminated (no message_stop).
fn anthropic_output(events: &[Value]) -> Vec<Value> {
    let stopped = events
        .iter()
        .any(|e| e.get("type").and_then(Value::as_str) == Some("message_stop"));
    if !stopped {
        return vec![];
    }
    let mut blocks: Vec<Value> = Vec::new();
    for e in events {
        match e.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                if let Some(b) = e.get("content_block") {
                    blocks.push(b.clone());
                }
            }
            Some("content_block_delta") => {
                let (Some(idx), Some(delta)) =
                    (e.get("index").and_then(Value::as_u64), e.get("delta"))
                else {
                    continue;
                };
                let Some(block) = blocks.get_mut(idx as usize) else {
                    continue;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => append_str(block, "text", delta.get("text")),
                    Some("thinking_delta") => append_str(block, "thinking", delta.get("thinking")),
                    Some("signature_delta") => {
                        append_str(block, "signature", delta.get("signature"))
                    }
                    Some("input_json_delta") => {
                        append_str(block, "partial_json", delta.get("partial_json"))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Finalize tool_use blocks: parse accumulated partial_json into `input`.
    for b in &mut blocks {
        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
            if let Some(pj) = b.get("partial_json").and_then(Value::as_str) {
                if let Ok(input) = serde_json::from_str::<Value>(pj) {
                    b["input"] = input;
                }
            }
            if let Some(obj) = b.as_object_mut() {
                obj.remove("partial_json");
            }
        }
    }
    if blocks.is_empty() {
        return vec![];
    }
    vec![serde_json::json!({"role": "assistant", "content": blocks})]
}

fn append_str(block: &mut Value, field: &str, addition: Option<&Value>) {
    let Some(add) = addition.and_then(Value::as_str) else {
        return;
    };
    let existing = block.get(field).and_then(Value::as_str).unwrap_or("");
    block[field] = Value::String(format!("{existing}{add}"));
}

/// Parse SSE text into data events (mirrors plant's parse_sse).
pub fn parse_sse(sse: &str) -> Vec<Value> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}
