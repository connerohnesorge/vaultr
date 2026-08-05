//! Learn state: immutable per-pass records inside each session's own capture
//! directory, folded with the frozen legacy ledger. One reader for Plant's
//! sweep and for vault validation, so learn state is never parsed twice.
//!
//! A record is `learn-<learner>-<host>-<YYYYMMDDTHHMMSSZ>.json`. The learner and
//! the writing host come from the path; content restates neither, so a record
//! cannot contradict its own location. Records are create-only and never
//! rewritten, so a resumed capture simply records a further pass.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// Learner labels a record filename may name. Mirrors `Harness::ledger_label`.
pub const LEARNERS: [&str; 2] = ["claude", "codex"];

/// The learner a legacy ledger row with no `learner` key belongs to.
pub const LEGACY_LEARNER: &str = "claude";

const PREFIX: &str = "learn-";

/// One recorded learn pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pass {
    pub processed_at: u64,
    pub outcome: String,
    pub learnings: Vec<String>,
}

/// Learner named by a learn-record filename, or None when the name is not a
/// learn record at all. Matches the `learn-<learner>-` prefix against the known
/// learner set — never splits on `-`, since hostnames contain them
/// (`allocator-vm-1`) and splitting misattributes the record.
pub fn record_learner(file_name: &str) -> Option<Result<&'static str>> {
    let rest = file_name.strip_prefix(PREFIX)?;
    if !file_name.ends_with(".json") {
        return None;
    }
    Some(
        LEARNERS
            .into_iter()
            .find(|learner| {
                rest.strip_prefix(learner)
                    .is_some_and(|tail| tail.starts_with('-'))
            })
            .with_context(|| format!("{file_name} names no known learner")),
    )
}

/// Parse one record's content. The path supplies learner and host; content
/// supplies only what happened.
pub fn parse_record(text: &str) -> Result<Pass> {
    let value: serde_json::Value = serde_json::from_str(text).context("record is not JSON")?;
    let processed_at = value
        .get("processed_at")
        .and_then(|at| at.as_str())
        .and_then(iso_to_epoch)
        .context("record has no readable ISO-8601 processed_at")?;
    let outcome = value
        .get("outcome")
        .and_then(|outcome| outcome.as_str())
        .context("record has no outcome")?
        .to_string();
    let learnings = value
        .get("learnings")
        .and_then(|learnings| learnings.as_array())
        .map(|learnings| {
            learnings
                .iter()
                .filter_map(|slug| slug.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(Pass {
        processed_at,
        outcome,
        learnings,
    })
}

/// Learner to latest pass, from one session directory's learn records.
///
/// An unreadable directory, an unknown learner, and a malformed record are all
/// errors: an empty fold means "no learner has processed this session", which
/// re-dispatches the whole corpus, so it must never stand in for a read failure.
pub fn session_passes(session_dir: &Path) -> Result<HashMap<String, Pass>> {
    let mut passes: HashMap<String, Pass> = HashMap::new();
    let entries = std::fs::read_dir(session_dir)
        .with_context(|| format!("read session directory {}", session_dir.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("read session directory {}", session_dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(learner) = record_learner(name) else {
            continue;
        };
        let path = entry.path();
        let learner = learner.with_context(|| format!("learn record {}", path.display()))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read learn record {}", path.display()))?;
        let pass =
            parse_record(&text).with_context(|| format!("learn record {}", path.display()))?;
        fold(&mut passes, learner, pass);
    }
    Ok(passes)
}

/// Session id to learner to latest pass, from the frozen legacy ledger.
///
/// Read once, in place. The file is history: its 138 duplicate keys and 518
/// learner-less rows are exempt from any rule the new records obey, and an
/// unparseable line is left to `validate` to report rather than failing the
/// fold. A missing ledger is an empty index, not an error.
pub fn legacy_index(content_root: &Path) -> Result<HashMap<String, HashMap<String, Pass>>> {
    let path = crate::validate::ledger_path(content_root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => bail!("read legacy ledger {}: {error}", path.display()),
    };
    let mut index: HashMap<String, HashMap<String, Pass>> = HashMap::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(sid) = value.get("session_id").and_then(|sid| sid.as_str()) else {
            continue;
        };
        let learner = value
            .get("learner")
            .and_then(|learner| learner.as_str())
            .unwrap_or(LEGACY_LEARNER);
        if !LEARNERS.contains(&learner) {
            continue;
        }
        let processed_at = value
            .get("processed_at")
            .and_then(|at| at.as_str())
            .and_then(iso_to_epoch)
            .unwrap_or(0);
        let outcome = value
            .get("outcome")
            .and_then(|outcome| outcome.as_str())
            .unwrap_or_default()
            .to_string();
        let learnings = value
            .get("learnings")
            .and_then(|learnings| learnings.as_array())
            .map(|learnings| {
                learnings
                    .iter()
                    .filter_map(|slug| slug.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        fold(
            index.entry(sid.to_string()).or_default(),
            learner,
            Pass {
                processed_at,
                outcome,
                learnings,
            },
        );
    }
    Ok(index)
}

/// Latest pass wins for a learner. Every consumer folds this way, so a session's
/// accumulated pass history costs storage only.
pub fn fold(passes: &mut HashMap<String, Pass>, learner: &str, pass: Pass) {
    match passes.get(learner) {
        Some(existing) if existing.processed_at >= pass.processed_at => {}
        _ => {
            passes.insert(learner.to_string(), pass);
        }
    }
}

/// ISO-8601 seconds (fractional seconds tolerated) to Unix epoch seconds.
pub fn iso_to_epoch(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let (year, month) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month - 3) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(dir: &Path, name: &str, at: &str) {
        std::fs::write(
            dir.join(name),
            format!(r#"{{"processed_at":"{at}","outcome":"learned","learnings":["a"]}}"#),
        )
        .unwrap();
    }

    #[test]
    fn latest_pass_wins_for_one_learner() {
        let dir = tempfile::tempdir().unwrap();
        record(
            dir.path(),
            "learn-claude-mac-20260101T000000Z.json",
            "2026-01-01T00:00:00Z",
        );
        record(
            dir.path(),
            "learn-claude-mac-20260202T000000Z.json",
            "2026-02-02T00:00:00Z",
        );
        let passes = session_passes(dir.path()).unwrap();
        assert_eq!(passes.len(), 1);
        assert_eq!(
            passes["claude"].processed_at,
            iso_to_epoch("2026-02-02T00:00:00Z").unwrap()
        );
    }

    #[test]
    fn host_containing_dashes_parses() {
        let dir = tempfile::tempdir().unwrap();
        record(
            dir.path(),
            "learn-codex-allocator-vm-1-20260101T000000Z.json",
            "2026-01-01T00:00:00Z",
        );
        let passes = session_passes(dir.path()).unwrap();
        assert_eq!(passes.keys().collect::<Vec<_>>(), vec!["codex"]);
    }

    #[test]
    fn unknown_learner_filename_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        record(
            dir.path(),
            "learn-gemini-mac-20260101T000000Z.json",
            "2026-01-01T00:00:00Z",
        );
        assert!(session_passes(dir.path()).is_err());
    }

    #[test]
    fn malformed_record_is_an_error_naming_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("learn-claude-mac-20260101T000000Z.json"),
            "{}",
        )
        .unwrap();
        let error = format!("{:#}", session_passes(dir.path()).unwrap_err());
        assert!(
            error.contains("learn-claude-mac-20260101T000000Z.json"),
            "{error}"
        );
    }

    #[test]
    fn unreadable_session_directory_is_an_error_not_an_empty_fold() {
        let dir = tempfile::tempdir().unwrap();
        assert!(session_passes(&dir.path().join("absent")).is_err());
    }

    #[test]
    fn non_learn_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".meta"), "{}").unwrap();
        std::fs::write(dir.path().join("learn-claude-mac.txt"), "x").unwrap();
        assert!(session_passes(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn legacy_rows_count_and_a_missing_learner_is_claude() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("learnings")).unwrap();
        std::fs::write(
            crate::validate::ledger_path(root.path()),
            concat!(
                r#"{"session_id":"s1","processed_at":"2026-01-01T00:00:00Z","outcome":"learned"}"#,
                "\n",
                r#"{"session_id":"s1","learner":"codex","processed_at":"2026-01-02T00:00:00Z","outcome":"skipped"}"#,
                "\n",
                "not json\n",
            ),
        )
        .unwrap();
        let index = legacy_index(root.path()).unwrap();
        assert_eq!(index["s1"].len(), 2);
        assert_eq!(index["s1"]["claude"].outcome, "learned");
        assert_eq!(index["s1"]["codex"].outcome, "skipped");
    }

    #[test]
    fn a_newer_record_supersedes_a_legacy_row() {
        let mut folded = HashMap::new();
        fold(
            &mut folded,
            "claude",
            Pass {
                processed_at: 100,
                outcome: "legacy".into(),
                learnings: vec![],
            },
        );
        fold(
            &mut folded,
            "claude",
            Pass {
                processed_at: 200,
                outcome: "record".into(),
                learnings: vec![],
            },
        );
        assert_eq!(folded["claude"].outcome, "record");
    }

    #[test]
    fn iso_to_epoch_parses_pass_timestamps() {
        assert_eq!(iso_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(iso_to_epoch("2000-01-01T00:00:00.123Z"), Some(946_684_800));
        assert_eq!(iso_to_epoch("not a date"), None);
        assert_eq!(iso_to_epoch(""), None);
    }

    #[test]
    fn missing_legacy_ledger_is_empty_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        assert!(legacy_index(root.path()).unwrap().is_empty());
    }
}
