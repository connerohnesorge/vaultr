use super::{apply_delta_transition, extract_response_output, Harness, ObservedMessage, Recon};
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Harness identity accumulated across a capture's envelopes.
#[derive(Clone, Copy)]
enum HarnessState {
    Unknown,
    ProvisionalCodex,
    Explicit(Harness),
}

impl HarnessState {
    fn value(self) -> Option<Harness> {
        match self {
            HarnessState::Unknown => None,
            HarnessState::ProvisionalCodex => Some(Harness::Codex),
            HarnessState::Explicit(harness) => Some(harness),
        }
    }
}

#[derive(Debug)]
struct Occurrence {
    message: Value,
    observed_at: Option<String>,
}

#[derive(Debug)]
struct PendingOutput {
    messages: Vec<Value>,
    observed_at: Option<String>,
}

/// Occurrence-preserving reconstruction state.
pub(super) struct ReconState {
    msgs: Vec<Value>,
    active: Vec<usize>,
    observed: Vec<Occurrence>,
    hash_dict: HashMap<String, Value>,
    partial: bool,
    key: String,
    harness: HarnessState,
    pending_output: Option<PendingOutput>,
    envelopes: usize,
}

impl ReconState {
    pub(super) fn new() -> Self {
        Self {
            msgs: Vec::new(),
            active: Vec::new(),
            observed: Vec::new(),
            hash_dict: HashMap::new(),
            partial: false,
            key: String::from("messages"),
            harness: HarnessState::Unknown,
            pending_output: None,
            envelopes: 0,
        }
    }

    /// Apply one parsed Envelope value: derive harness, track its completed
    /// response, and replay its request-history transition.
    pub(super) fn apply(&mut self, env: &Value) -> Result<()> {
        let explicit_harness = env
            .get("harness")
            .and_then(Value::as_str)
            .and_then(Harness::from_label);
        let history = env.pointer("/request/body_delta/history");
        let provisional_codex = history
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            == Some("input");
        let next_harness = match (self.harness, explicit_harness, provisional_codex) {
            (HarnessState::Explicit(first), Some(next), _) if first != next => {
                anyhow::bail!("conflicting explicit harness labels");
            }
            (_, Some(harness), _) => HarnessState::Explicit(harness),
            (HarnessState::Unknown, None, true) => HarnessState::ProvisionalCodex,
            (state, None, _) => state,
        };
        let harness = next_harness.value();
        let observed_at = env
            .get("observed_at")
            .and_then(Value::as_str)
            .map(String::from);

        if let Some(history) = history {
            match apply_delta_transition(history, &mut self.msgs, &mut self.hash_dict) {
                Ok(transition) => {
                    self.observe_transition(transition.retained_prefix, observed_at.clone());
                    if let Some(key) = history.get("key").and_then(Value::as_str) {
                        self.key = key.to_string();
                    }
                }
                Err(error) if error.recoverable() && !self.msgs.is_empty() => {
                    self.flush_pending_output();
                    self.partial = true;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            self.flush_pending_output();
        }

        self.harness = next_harness;
        self.envelopes += 1;
        let mut output = extract_response_output(env, harness);
        // Codex stamps each replayed item with the turn it belongs to; the
        // request-side items carry it already, but response-side items do not.
        if harness == Some(Harness::Codex) {
            if let Some(turn_id) = env.get("turn_id").and_then(Value::as_str) {
                for item in &mut output {
                    if let Some(object) = item.as_object_mut() {
                        object.insert(
                            "internal_chat_message_metadata_passthrough".into(),
                            serde_json::json!({ "turn_id": turn_id }),
                        );
                    }
                }
            }
        }
        self.pending_output = (!output.is_empty()).then_some(PendingOutput {
            messages: output,
            observed_at,
        });
        Ok(())
    }

    fn observe_transition(&mut self, retained_prefix: usize, observed_at: Option<String>) {
        self.active.truncate(retained_prefix);
        let suffix = self.msgs[retained_prefix..].to_vec();
        let pending_match = self.pending_output.as_ref().and_then(|pending| {
            contiguous_subsequence(&suffix, &pending.messages).map(|start| {
                (
                    start,
                    start + pending.messages.len(),
                    pending.observed_at.clone(),
                )
            })
        });
        if pending_match.is_none() {
            self.flush_pending_output();
        } else {
            self.pending_output = None;
        }
        for (index, message) in suffix.into_iter().enumerate() {
            let timestamp = pending_match
                .as_ref()
                .filter(|(start, end, _)| index >= *start && index < *end)
                .and_then(|(_, _, timestamp)| timestamp.clone())
                .or_else(|| observed_at.clone());
            self.observed.push(Occurrence {
                message,
                observed_at: timestamp,
            });
            self.active.push(self.observed.len() - 1);
        }
    }

    fn flush_pending_output(&mut self) {
        let Some(pending) = self.pending_output.take() else {
            return;
        };
        for message in pending.messages {
            self.observed.push(Occurrence {
                message,
                observed_at: pending.observed_at.clone(),
            });
        }
    }

    pub(super) fn finish(mut self) -> Recon {
        let history_len = self.active.len();
        let trailing_appended = self
            .pending_output
            .as_ref()
            .map_or(0, |pending| pending.messages.len());
        if let Some(pending) = self.pending_output.take() {
            for message in pending.messages {
                self.observed.push(Occurrence {
                    message,
                    observed_at: pending.observed_at.clone(),
                });
                self.active.push(self.observed.len() - 1);
            }
        }
        let final_occurrences: HashSet<usize> = self.active.iter().copied().collect();
        let messages = self
            .active
            .iter()
            .map(|index| self.observed[*index].message.clone())
            .collect();
        let observed_messages = self
            .observed
            .into_iter()
            .enumerate()
            .map(|(index, occurrence)| ObservedMessage {
                message: occurrence.message,
                in_final_replay: final_occurrences.contains(&index),
                observed_at: occurrence.observed_at,
            })
            .collect();
        Recon {
            key: self.key,
            harness: self.harness.value(),
            history_len,
            messages,
            observed_messages,
            partial: self.partial,
            trailing_appended,
            envelopes: self.envelopes,
        }
    }
}

fn contiguous_subsequence(haystack: &[Value], needle: &[Value]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
