use std::fs;
use std::path::PathBuf;
use vaultr::{normalize, recon, render};

fn base() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn check_golden(fixture: &str, golden: &str) {
    let r = recon::reconstruct(&base().join("fixtures").join(fixture)).unwrap();
    let md = render::markdown(&normalize::normalize(&r.messages));
    let golden_path = base().join("golden").join(golden);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        fs::write(&golden_path, &md).unwrap();
    }
    let expected = fs::read_to_string(&golden_path)
        .unwrap_or_else(|_| panic!("missing golden {golden}; run with UPDATE_GOLDEN=1"));
    assert_eq!(md, expected, "golden mismatch for {golden}");
}

#[test]
fn golden_claude_append() {
    check_golden("claude_append.jsonl", "claude_append.md");
}

#[test]
fn golden_codex_append() {
    check_golden("codex_append.jsonl", "codex_append.md");
}

#[test]
fn golden_compaction() {
    check_golden("compaction.jsonl", "compaction.md");
}

#[test]
fn markdown_has_no_tool_results_or_thinking() {
    let r = recon::reconstruct(&base().join("fixtures/claude_append.jsonl")).unwrap();
    let md = render::markdown(&normalize::normalize(&r.messages));
    assert!(!md.contains("file contents\n"), "tool result leaked");
    assert!(!md.contains("hmm"), "thinking leaked");
    assert!(!md.contains("system-reminder"));
    assert!(md.contains("## User"));
    assert!(md.contains("## Assistant"));
    assert!(md.contains("> `Read`"));
}
