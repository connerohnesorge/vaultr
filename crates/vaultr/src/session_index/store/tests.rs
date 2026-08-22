use super::*;

#[test]
fn metadata_version_controls_compatibility() {
    assert!(Metadata::current().compatible());
    assert!(!Metadata {
        schema_version: 0,
        ..Metadata::default()
    }
    .compatible());
}

#[test]
fn query_normalization_keeps_unknown_prefixes_literal() {
    assert_eq!(normalize_query("cwd:/work"), "cwd:/work");
    assert_eq!(normalize_query("paste:thing"), "\"paste:thing\"");
    assert_eq!(normalize_query("plain bad:value"), "plain \"bad:value\"");
}

/// A sealed capture carrying one unparseable record must cost only its own
/// session. 15 of 6981 seals are already committed broken (plant's scrubber ate
/// the backslash of an escaped quote), and cancelling on the first of them left
/// `vaultr session index` failing forever over 0.2% of the corpus.
#[test]
fn one_corrupt_seal_does_not_strand_the_rest_of_the_corpus() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();

    // Shaped like the real breakage: the `\` before the closing quote is gone,
    // so the bare `"` ends the string early and the record stops parsing.
    let corrupt = seal(
        root,
        "corrupt",
        "{\"type\":\"request\",\"body\":\"X-API-Key: [REDACTED]\" trailing\"}",
    );
    let readable_one = seal(root, "readable-one", "");
    let readable_two = seal(root, "readable-two", "");

    let sources = vec![
        source("corrupt", corrupt),
        source("readable-one", readable_one),
        source("readable-two", readable_two),
    ];

    let mut visited = Vec::new();
    let unreadable = decode_sessions(sources, 2, |source, _| {
        visited.push(source.session.id);
        Ok(())
    })
    .expect("a corrupt seal must not fail the run");

    visited.sort();
    assert_eq!(visited, vec!["readable-one", "readable-two"]);
    assert_eq!(unreadable.len(), 1, "{unreadable:?}");
    assert_eq!(unreadable[0].id, "corrupt");
    // Control: the skip must come from the record being unparseable, not from
    // an unreadable or non-zstd fixture that would pass this test for free.
    let reason = unreadable[0].reason.to_ascii_lowercase();
    assert!(
        reason.contains("json") || reason.contains("malformed"),
        "expected a JSON parse failure, got: {}",
        unreadable[0].reason
    );
}

/// The other half of the contract: an index-writer refusal is still fatal,
/// because that leaves the index WRONG rather than merely missing a session.
#[test]
fn a_visit_failure_is_still_fatal() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let sources = vec![
        source("one", seal(root, "one", "")),
        source("two", seal(root, "two", "")),
    ];
    let error = decode_sessions(sources, 1, |_, _| anyhow::bail!("writer refused"))
        .expect_err("a visit failure must abort");
    assert!(format!("{error:#}").contains("writer refused"));
}

fn seal(root: &Path, name: &str, extra_record: &str) -> std::path::PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let mut body = String::from(
        "{\"schema_version\":1,\"harness\":\"claude\",\"session_id\":\"s\",\"request_body\":{}}\n",
    );
    if !extra_record.is_empty() {
        body.push_str(extra_record);
        body.push('\n');
    }
    let path = directory.join("turns.jsonl.zst");
    std::fs::write(&path, zstd::encode_all(body.as_bytes(), 3).unwrap()).unwrap();
    path
}

fn source(id: &str, capture: std::path::PathBuf) -> SessionSource {
    SessionSource {
        session: crate::vault::Session {
            id: id.to_string(),
            meta: crate::vault::Meta::default(),
        },
        capture,
        fingerprint: format!("{id}-fingerprint"),
    }
}

#[test]
fn incompatible_metadata_rebuilds_the_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("index");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("stale"), "old data").unwrap();
    std::fs::write(directory.join(METADATA_FILE), r#"{"schema_version":0}"#).unwrap();
    let (metadata, rebuilt) = prepare_directory(&directory).unwrap();
    assert!(rebuilt);
    assert!(metadata.compatible());
    assert!(!directory.join("stale").exists());
}
