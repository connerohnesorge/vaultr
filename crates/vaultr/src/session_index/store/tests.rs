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
