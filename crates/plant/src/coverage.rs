//! Read-only capture coverage over Plant's observation window.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Capture completeness for one Session Capture, measured over Plant's observation
/// window (see ADR-0001). A resumed session's pre-window transcript history is
/// reported as `carryover`, never as loss.
#[derive(Debug)]
pub struct Coverage {
    pub sid: String,
    /// Earliest captured `observed_at`, else meta `original_start`.
    pub window_start: String,
    pub resumed: bool,
    /// Distinct native assistant `requestId`s at or after the window start.
    pub in_window_native: usize,
    /// Distinct captured Envelope `request-id`s.
    pub captured: usize,
    /// Native `requestId`s predating the window (out-of-scope, not lost).
    pub carryover: usize,
    /// In-window native `requestId`s with no captured Envelope, sorted.
    pub missing: Vec<String>,
    /// Turns Plant recorded as dropped in `.meta`. Non-zero proves the capture
    /// is incomplete without any native-transcript comparison.
    pub recorded_drops: u64,
}

impl Coverage {
    /// In-window captured / in-window native, as a percentage.
    pub fn pct(&self) -> f64 {
        let hit = self.in_window_native - self.missing.len();
        hit as f64 * 100.0 / self.in_window_native as f64
    }
}

/// Compute [`Coverage`] for a session id (or unambiguous prefix). Read-only over the
/// Session Capture and the harness transcript named in meta; mutates nothing.
pub fn coverage(vault: &Path, query: &str) -> Result<Coverage, String> {
    let session = vaultr::vault::resolve_id(vault, query).map_err(|e| e.to_string())?;
    let dir = vaultr::vault::session_dir(vault, &session).map_err(|e| e.to_string())?;
    let cap = vaultr::vault::capture_file(&dir).map_err(|e| e.to_string())?;

    // Captured side: distinct response request-ids, and the window start (min observed_at).
    let mut captured: HashSet<String> = HashSet::new();
    let mut window_start: Option<String> = None;
    let mut harness = None;
    vaultr::recon::for_each_envelope(&cap, |env| {
        harness = vaultr::recon::Harness::from_envelope(env, harness);
        if let Some(rid) = env
            .pointer("/response/headers/request-id")
            .and_then(|v| v.as_str())
        {
            captured.insert(rid.to_string());
        }
        if let Some(obs) = env.get("observed_at").and_then(|v| v.as_str()) {
            if window_start.as_deref().is_none_or(|w| obs < w) {
                window_start = Some(obs.to_string());
            }
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    if harness == Some(vaultr::recon::Harness::Codex) {
        return Err(format!(
            "coverage unsupported for Codex capture {}: no comparable native request IDs",
            session.id
        ));
    }
    let window_start = window_start
        .or_else(|| session.meta.original_start.clone())
        .ok_or_else(|| format!("no envelopes and no original_start for {}", session.id))?;

    // Native side: assistant requestId -> whether its first occurrence predates the window.
    let transcript = session
        .meta
        .transcript_path
        .clone()
        .ok_or_else(|| format!("no transcript_path in meta for {}", session.id))?;
    let file = std::fs::File::open(&transcript)
        .map_err(|e| format!("read transcript {transcript}: {e}"))?;
    let mut first_seen: HashMap<String, bool> = HashMap::new();
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line =
            line.map_err(|e| format!("read transcript {transcript} line {}: {e}", line_no + 1))?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(rid) = v.get("requestId").and_then(|r| r.as_str()) else {
            continue;
        };
        let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
        let predates_window = ts < window_start.as_str();
        first_seen
            .entry(rid.to_string())
            .and_modify(|seen_before| *seen_before |= predates_window)
            .or_insert(predates_window);
    }
    let mut in_window_native = 0usize;
    let mut carryover = 0usize;
    let mut missing = vec![];
    for (rid, predates_window) in &first_seen {
        if *predates_window {
            carryover += 1;
        } else {
            in_window_native += 1;
            if !captured.contains(rid) {
                missing.push(rid.clone());
            }
        }
    }
    if in_window_native == 0 {
        return Err(format!(
            "no comparable in-window native request IDs for {}",
            session.id
        ));
    }
    missing.sort();

    Ok(Coverage {
        recorded_drops: session.meta.dropped_turns,
        sid: session.id,
        window_start,
        resumed: session.meta.session_start_source.as_deref() == Some("resume"),
        in_window_native,
        captured: captured.len(),
        carryover,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Build a minimal vault: meta index + dated session dir with turns.jsonl + a
    /// Claude transcript. `envelopes` and `transcript` are raw file bodies.
    fn fixture(label: &str, resumed: bool, envelopes: &str, transcript: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("plant-cov-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sid = "cov00000-0000-4000-8000-000000000000";
        let start = "2026-07-17T19:00:00.000Z";
        let dir = root.join("2026/07/17").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("turns.jsonl"), envelopes).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, transcript).unwrap();
        std::fs::create_dir_all(root.join(".meta")).unwrap();
        let source = if resumed { "resume" } else { "wire" };
        std::fs::write(
            root.join(".meta").join(format!("{sid}.json")),
            format!(
                r#"{{"session_id":"{sid}","original_start":"{start}","session_start_source":"{source}","transcript_path":"{}"}}"#,
                transcript_path.display()
            ),
        )
        .unwrap();
        root
    }

    fn envelope(observed_at: &str, request_id: &str) -> String {
        envelope_for("claude-code", observed_at, request_id)
    }

    fn envelope_for(harness: &str, observed_at: &str, request_id: &str) -> String {
        format!(
            r#"{{"harness":"{harness}","observed_at":"{observed_at}","response":{{"headers":{{"request-id":"{request_id}"}}}}}}"#
        )
    }

    fn assistant(ts: &str, request_id: &str) -> String {
        format!(r#"{{"type":"assistant","requestId":"{request_id}","timestamp":"{ts}"}}"#)
    }

    #[test]
    fn resume_carryover_is_not_loss() {
        let envelopes = format!("{}\n", envelope("2026-07-17T19:18:00.000Z", "req_A"));
        let transcript = format!(
            "{}\n{}\n{}\n",
            assistant("2026-07-17T18:23:00.000Z", "req_OLD1"),
            assistant("2026-07-17T18:24:00.000Z", "req_OLD2"),
            assistant("2026-07-17T19:18:00.000Z", "req_A"),
        );
        let root = fixture("carryover", true, &envelopes, &transcript);
        let result = coverage(&root, "cov00000").unwrap();
        assert!(result.resumed);
        assert_eq!(result.window_start, "2026-07-17T19:18:00.000Z");
        assert_eq!(result.carryover, 2, "pre-window ids are carryover");
        assert_eq!(result.in_window_native, 1);
        assert!(result.missing.is_empty(), "in-window fully captured");
        assert_eq!(result.pct(), 100.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn full_window_is_complete() {
        let envelopes = format!(
            "{}\n{}\n",
            envelope("2026-07-17T19:18:00.000Z", "req_A"),
            envelope("2026-07-17T19:19:00.000Z", "req_B"),
        );
        let transcript = format!(
            "{}\n{}\n",
            assistant("2026-07-17T19:18:00.000Z", "req_A"),
            assistant("2026-07-17T19:19:00.000Z", "req_B"),
        );
        let root = fixture("full", false, &envelopes, &transcript);
        let result = coverage(&root, "cov00000").unwrap();
        assert_eq!(result.in_window_native, 2);
        assert!(result.missing.is_empty());
        assert_eq!(result.pct(), 100.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reports_genuine_in_window_gap() {
        let envelopes = format!("{}\n", envelope("2026-07-17T19:18:00.000Z", "req_A"));
        let transcript = format!(
            "{}\n{}\n",
            assistant("2026-07-17T19:18:00.000Z", "req_A"),
            assistant("2026-07-17T19:20:00.000Z", "req_B"),
        );
        let root = fixture("gap", false, &envelopes, &transcript);
        let result = coverage(&root, "cov00000").unwrap();
        assert_eq!(result.in_window_native, 2);
        assert_eq!(result.missing, vec!["req_B".to_string()]);
        assert_eq!(result.pct(), 50.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn streams_every_mixed_generation_from_any_sibling() {
        let raw = format!(
            "{}\n{{\"harness\":\"claude-code\"",
            envelope("2026-07-17T19:20:00.000Z", "req_D")
        );
        let transcript = [
            "req_A",
            "req_B",
            "req_C",
            "req_D",
            "req_MISSING_1",
            "req_MISSING_2",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, rid)| assistant(&format!("2026-07-17T19:{i:02}:00.000Z"), rid))
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let root = fixture("mixed", true, &raw, &transcript);
        let dir = root.join("2026/07/17/cov00000-0000-4000-8000-000000000000");
        let sealed = dir.join("turns.jsonl.zst");
        let sealed_body = format!(
            "{}{}\n",
            envelope("2026-07-17T19:00:00.000Z", "req_A"),
            envelope("2026-07-17T19:10:00.000Z", "req_B"),
        );
        let sealed_body = zstd::encode_all(sealed_body.as_bytes(), 1).unwrap();
        std::fs::write(&sealed, &sealed_body).unwrap();
        let detached_body = envelope("2026-07-17T19:15:00.000Z", "req_C") + "\n";
        let detached = dir.join(format!(
            "turns.jsonl.sealing-{}-{}",
            sealed_body.len(),
            vaultr::digest::sha256_hex(detached_body.as_bytes())
        ));
        std::fs::write(&detached, detached_body).unwrap();

        for entry in [&dir.join("turns.jsonl"), &sealed, &detached] {
            let mut ids = vec![];
            vaultr::recon::for_each_envelope(entry, |env| {
                if let Some(id) = env
                    .pointer("/response/headers/request-id")
                    .and_then(serde_json::Value::as_str)
                {
                    ids.push(id.to_string());
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(ids, ["req_A", "req_B", "req_C", "req_D"]);
        }

        let result = coverage(&root, "cov00000").unwrap();
        assert_eq!(result.captured, 4);
        assert_eq!(
            result.missing,
            ["req_MISSING_1".to_string(), "req_MISSING_2".to_string()]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_malformed_capture_evidence() {
        let root = fixture(
            "malformed",
            false,
            "",
            &format!("{}\n", assistant("2026-07-17T19:00:00.000Z", "req_A")),
        );
        let sealed = root.join("2026/07/17/cov00000-0000-4000-8000-000000000000/turns.jsonl.zst");
        std::fs::write(
            sealed,
            zstd::encode_all(&b"{\"harness\":\"claude-code\"\n"[..], 1).unwrap(),
        )
        .unwrap();
        let error = coverage(&root, "cov00000").unwrap_err();
        assert!(error.contains("sealed record 1"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unreadable_capture_evidence() {
        use std::os::unix::fs::PermissionsExt;

        let root = fixture(
            "unreadable",
            false,
            &(envelope("2026-07-17T19:00:00.000Z", "req_A") + "\n"),
            &(assistant("2026-07-17T19:00:00.000Z", "req_A") + "\n"),
        );
        let raw = root.join("2026/07/17/cov00000-0000-4000-8000-000000000000/turns.jsonl");
        std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o000)).unwrap();
        let error = coverage(&root, "cov00000").unwrap_err();
        std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(error.contains("open"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_codex_and_empty_denominators() {
        let codex = fixture(
            "codex",
            false,
            &(envelope_for("codex", "2026-07-17T19:00:00.000Z", "req_A") + "\n"),
            "{\"type\":\"response_item\",\"timestamp\":\"2026-07-17T19:00:00.000Z\"}\n",
        );
        let error = coverage(&codex, "cov00000").unwrap_err();
        assert!(error.contains("unsupported for Codex"), "{error}");
        let _ = std::fs::remove_dir_all(codex);

        let empty = fixture(
            "empty",
            false,
            &(envelope("2026-07-17T19:00:00.000Z", "req_A") + "\n"),
            "{\"type\":\"user\",\"timestamp\":\"2026-07-17T19:00:00.000Z\"}\n",
        );
        let error = coverage(&empty, "cov00000").unwrap_err();
        assert!(
            error.contains("no comparable in-window native request IDs"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn handles_large_records() {
        const RECORDS: usize = 2;
        const PADDING: usize = 8 * 1024 * 1024;
        let root = fixture("large-record", false, "", "");
        let dir = root.join("2026/07/17/cov00000-0000-4000-8000-000000000000");
        let padding = "x".repeat(PADDING);
        {
            let mut capture =
                std::io::BufWriter::new(std::fs::File::create(dir.join("turns.jsonl")).unwrap());
            let mut transcript = std::io::BufWriter::new(
                std::fs::File::create(root.join("transcript.jsonl")).unwrap(),
            );
            for i in 0..RECORDS {
                writeln!(
                    capture,
                    r#"{{"harness":"claude-code","observed_at":"2026-07-17T19:00:00.000Z","padding":"{padding}","response":{{"headers":{{"request-id":"req_{i}"}}}}}}"#
                )
                .unwrap();
                writeln!(
                    transcript,
                    r#"{{"type":"assistant","requestId":"req_{i}","timestamp":"2026-07-17T19:00:00.000Z","padding":"{padding}"}}"#
                )
                .unwrap();
            }
        }

        let result = coverage(&root, "cov00000").unwrap();
        assert_eq!(result.in_window_native, RECORDS);
        assert_eq!(result.captured, RECORDS);
        assert!(result.missing.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
