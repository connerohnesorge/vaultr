//! Streaming reconstruction of the final message history from a turns.jsonl
//! (raw or zstd) capture, plus the `body_delta` encoder (`encode_delta`) that
//! plant uses at capture time — encode and apply live side by side so they
//! can't drift. Memory is bounded by the largest single envelope plus the
//! final history — the archive is never loaded whole.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// Harness identity of a capture, derived once during reconstruction.
///
/// Precedence: the envelope `harness` field is ground truth — envelopes are
/// the captured wire truth, whereas `.meta/<id>.json` is a mutable
/// hook-merged sidecar that can go stale or be wrong. When envelopes lack
/// the field, a history key of "input" resolves to Codex. Only when neither
/// resolves (`Recon::harness` is `None`) may callers fall back to
/// meta.harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    /// Map a recorded harness label to an identity. Accepts the "claude"
    /// alias alongside the canonical "claude-code"; unknown labels resolve
    /// nothing so callers fall through to their next source.
    pub fn from_label(label: &str) -> Option<Harness> {
        match label {
            "codex" => Some(Harness::Codex),
            "claude-code" | "claude" => Some(Harness::Claude),
            _ => None,
        }
    }
}

/// Result of reconstructing a capture.
pub struct Recon {
    /// History key seen on the wire: "messages" (Anthropic) or "input" (Codex).
    pub key: String,
    /// Harness identity derived from the envelopes (see [`Harness`] for the
    /// precedence rules). `None` for degenerate captures where neither the
    /// envelope `harness` field nor the history key resolved.
    pub harness: Option<Harness>,
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
///
/// A resumed capture can have a sealed generation followed by a raw one.
/// Entering through either canonical sibling reconstructs both in that order.
pub fn reconstruct(path: &Path) -> Result<Recon> {
    let sibling = match path.file_name().and_then(|name| name.to_str()) {
        Some("turns.jsonl") => Some(path.with_file_name("turns.jsonl.zst")),
        Some("turns.jsonl.zst") => Some(path.with_file_name("turns.jsonl")),
        _ => None,
    };
    if let Some(sibling) = sibling {
        if sibling
            .try_exists()
            .with_context(|| format!("inspect {}", sibling.display()))?
        {
            let (sealed, raw) =
                if path.file_name().and_then(|name| name.to_str()) == Some("turns.jsonl.zst") {
                    (path, sibling.as_path())
                } else {
                    (sibling.as_path(), path)
                };
            let sealed =
                File::open(sealed).with_context(|| format!("open {}", sealed.display()))?;
            let raw = File::open(raw).with_context(|| format!("open {}", raw.display()))?;
            let dec = zstd::Decoder::new(sealed).context("zstd decoder")?;
            return reconstruct_reader(dec.chain(raw));
        }
    }

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
    let mut harness: Option<Harness> = None;
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
        // Derive harness identity once, envelope-first: the envelope field is
        // the captured wire truth; key == "input" resolves Codex only while
        // no envelope has said otherwise.
        match env
            .get("harness")
            .and_then(Value::as_str)
            .and_then(Harness::from_label)
        {
            Some(h) => harness = Some(h),
            None => {
                if harness.is_none()
                    && env
                        .pointer("/request/body_delta/history/key")
                        .and_then(Value::as_str)
                        == Some("input")
                {
                    harness = Some(Harness::Codex);
                }
            }
        }
        // The response of every envelope *before* the last is reflected in the
        // next request's history delta; only the final envelope's completed
        // response needs appending. Track it per-envelope, keeping only the last.
        trailing = extract_response_output(&env, harness);
        // Codex stamps each replayed item with the turn it belongs to; the
        // request-side items of this turn carry it already (baked into the
        // wire), but the response-side items we append here don't — add it so
        // a fork's resume replays them byte-identically to a native resume.
        if harness == Some(Harness::Codex) {
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
        harness,
        history_len,
        messages: msgs,
        trailing_appended,
        envelopes,
    })
}

/// commonPrefix: element-wise JSON equality.
fn common_prefix(a: &[Value], b: &[Value]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Encode `body` against the prior request body as the on-disk `body_delta`
/// shape: `{set, remove, history: {key, prefix_length, append}}`. For object
/// bodies whose history key is an array, `apply_delta` (history) plus
/// set/remove replay reconstructs `body` exactly; degenerate bodies
/// (non-object, non-array history) encode as an empty delta and rely on
/// state.json for ground truth. `big_fields` are stored only when changed;
/// everything else is verbatim.
pub fn encode_delta(prior: &Value, body: &Value, history_key: &str, big_fields: &[&str]) -> Value {
    let empty = vec![];
    let history = body
        .get(history_key)
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let prior_history = prior
        .get(history_key)
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let prefix = common_prefix(prior_history, history);

    let mut set = Map::new();
    let mut remove: Vec<String> = vec![];
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            if k == history_key {
                continue;
            }
            if big_fields.contains(&k.as_str()) {
                if prior.get(k) != Some(v) {
                    set.insert(k.clone(), v.clone());
                }
            } else {
                set.insert(k.clone(), v.clone());
            }
        }
        if let Some(pobj) = prior.as_object() {
            for k in pobj.keys() {
                if !obj.contains_key(k) && k != history_key {
                    remove.push(k.clone());
                }
            }
        }
    }

    json!({
        "set": set,
        "remove": remove,
        "history": { "key": history_key, "prefix_length": prefix, "append": history[prefix..] },
    })
}

/// Apply one history delta (append or content-addressed form), mirroring recon.mjs.
pub fn apply_delta(h: &Value, msgs: &mut Vec<Value>, hash_dict: &mut HashMap<String, Value>) {
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
fn extract_response_output(env: &Value, harness: Option<Harness>) -> Vec<Value> {
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
    if harness == Some(Harness::Codex) {
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

/// Parse SSE text into JSON data events, ignoring terminal and malformed lines.
pub fn parse_sse(sse: &str) -> Vec<Value> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only inverse of `encode_delta`: replay set/remove over the prior
    /// body and `apply_delta` over its history.
    fn apply_body(prior: &Value, delta: &Value, history_key: &str) -> Value {
        let mut out = prior.as_object().cloned().unwrap_or_default();
        for (k, v) in delta["set"].as_object().unwrap() {
            out.insert(k.clone(), v.clone());
        }
        for k in delta["remove"].as_array().unwrap() {
            out.remove(k.as_str().unwrap());
        }
        let mut msgs: Vec<Value> = prior
            .get(history_key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut dict = HashMap::new();
        apply_delta(&delta["history"], &mut msgs, &mut dict);
        out.insert(history_key.to_string(), Value::Array(msgs));
        Value::Object(out)
    }

    fn msg(i: u64) -> Value {
        json!({ "role": if i.is_multiple_of(2) { "user" } else { "assistant" }, "content": format!("m{i}") })
    }

    #[test]
    fn encode_apply_round_trip_property() {
        const BIG: &[&str] = &["tools", "system"];
        // Deterministic LCG-driven generator: histories that share prefixes,
        // grow, compact, and diverge; big/small fields that change or vanish.
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut rand = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut prior = json!({});
        for _ in 0..200 {
            let mut body = Map::new();
            body.insert("model".into(), json!(format!("m{}", rand(3))));
            if rand(4) > 0 {
                body.insert("tools".into(), json!([{ "name": format!("t{}", rand(2)) }]));
            }
            if rand(4) > 0 {
                body.insert("system".into(), json!(format!("sys{}", rand(2))));
            }
            if rand(3) > 0 {
                body.insert("temperature".into(), json!(rand(10)));
            }
            let prior_len = prior
                .get("messages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64;
            let keep = rand(prior_len + 1); // 0..=prior_len shared prefix
            let grow = rand(4);
            let history: Vec<Value> = (0..keep)
                .chain((100 + keep)..(100 + keep + grow)) // diverging tail
                .map(msg)
                .collect();
            body.insert("messages".into(), Value::Array(history));
            let body = Value::Object(body);

            let delta = encode_delta(&prior, &body, "messages", BIG);
            assert_eq!(
                apply_body(&prior, &delta, "messages"),
                body,
                "prior={prior} body={body} delta={delta}"
            );
            prior = body;
        }
    }

    #[test]
    fn encode_delta_round_trip_big_field_set_and_remove() {
        const BIG: &[&str] = &["tools", "system"];
        let prior = json!({
            "model": "m",
            "system": "sys",
            "tools": [{ "name": "t" }],
            "temperature": 0.5,
            "messages": [msg(0), msg(1)],
        });
        // Big field `tools` changes, `system` disappears, small field
        // `temperature` disappears; history compacts to a diverging singleton.
        let body = json!({
            "model": "m",
            "tools": [{ "name": "t2" }],
            "messages": [msg(7)],
        });
        let delta = encode_delta(&prior, &body, "messages", BIG);
        assert_eq!(delta["set"]["tools"], body["tools"]);
        assert!(delta["set"].get("system").is_none());
        let removed = delta["remove"].as_array().unwrap();
        assert!(removed.contains(&json!("system")));
        assert!(removed.contains(&json!("temperature")));
        assert_eq!(delta["history"]["prefix_length"], 0);
        assert_eq!(apply_body(&prior, &delta, "messages"), body);

        // Unchanged big field: absent from `set`, restored from prior on apply.
        let body2 = json!({
            "model": "m",
            "tools": [{ "name": "t" }],
            "messages": [msg(0), msg(1), msg(2)],
        });
        let delta2 = encode_delta(&prior, &body2, "messages", BIG);
        assert!(delta2["set"].get("tools").is_none());
        assert_eq!(delta2["history"]["prefix_length"], 2);
        // `system` was dropped, `tools` kept-but-deduped: apply must restore
        // exactly body2, tools included.
        assert_eq!(apply_body(&prior, &delta2, "messages"), body2);
    }
}
