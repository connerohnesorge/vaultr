use std::fs;
use vaultr::validate;

fn write(root: &std::path::Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

#[test]
fn validate_fixture_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let present = "11111111-1111-1111-1111-111111111111";
    let inline_missing = "22222222-2222-2222-2222-222222222222";
    let bare_missing = "33333333-3333-3333-3333-333333333333";
    let outside_sources = "44444444-4444-4444-4444-444444444444";
    write(root, &format!("sessions/.meta/{present}.json"), "{}");
    write(
        root,
        "learnings/good-note.md",
        "---\nname: good-note\ndescription: fine\ntype: knowledge\n---\nSee [[other-note]].\n",
    );
    write(
        root,
        "learnings/other-note.md",
        "---\nname: other-note\ndescription: also fine\ntype: rule\n---\nbody\n",
    );
    // broken wikilink = error
    write(
        root,
        "learnings/broken.md",
        "---\nname: broken\ndescription: d\ntype: gotcha\n---\nSee [[does-not-exist]].\n",
    );
    // code fence + inline code literals must NOT flag; ignore marker must not flag
    write(
        root,
        "learnings/literals.md",
        "---\nname: literals\ndescription: d\ntype: knowledge\n---\n```toml\n[[bin]]\nname = \"x\"\n```\nand `[[inline-literal]]` and [[skipme]] <!-- vault-validate: ignore -->\nbash: [[ $(kubectl get) ]] and [[:cntrl:]]\n",
    );
    // missing frontmatter = warning; bad md path = error
    write(
        root,
        "runbooks/no-fm.md",
        "no frontmatter here\n[dead](/learnings/nope.md)\n[alive](/learnings/good-note.md)\n",
    );
    // same slug in two content dirs = error (ambiguous bare [[wikilink]])
    write(
        root,
        "incidents/good-note.md",
        "---\ntype: Incident\ntitle: collides\n---\nbody\n",
    );
    write(
        root,
        "learnings/inline-sources.md",
        &format!(
            "---\nname: inline-sources\ndescription: fine\ntype: knowledge\n\
             sources: [sessions/2026/07/20/{present}, \"sessions/2026/07/20/{inline_missing}\", \
             sessions/2026/7/20/{outside_sources}, sessions/2026/07/20/{outside_sources}/extra]\n\
             unrelated: sessions/2026/07/20/{outside_sources}\n---\nbody\n"
        ),
    );
    write(
        root,
        "learnings/bare-sources.md",
        &format!(
            "---\nname: bare-sources\ndescription: fine\ntype: knowledge\nsources:\n\
             - {present}\n  - {bare_missing}\n---\nbody\n"
        ),
    );
    // corrupt ledger line = error
    write(
        root,
        "learnings/.ledger.jsonl",
        "{\"session_id\":\"abc\"}\nnot json\n",
    );

    let report = validate::scan(root).unwrap();
    let errs: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.severity == validate::Severity::Error)
        .collect();
    assert_eq!(errs.len(), 4, "findings: {:#?}", report.findings);
    assert!(errs
        .iter()
        .any(|f| f.kind == "duplicate-slug" && f.detail.contains("good-note")));
    assert!(errs
        .iter()
        .any(|f| f.kind == "wikilink" && f.detail.contains("does-not-exist")));
    assert!(errs
        .iter()
        .any(|f| f.kind == "mdpath" && f.detail.contains("/learnings/nope.md")));
    assert!(errs.iter().any(|f| f.kind == "ledger" && f.line == 2));
    assert!(!report.findings.iter().any(|f| {
        f.detail.contains("bin")
            || f.detail.contains("inline-literal")
            || f.detail.contains("skipme")
            || f.detail.contains("cntrl")
            || f.detail.contains("kubectl")
    }));
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == "frontmatter" && f.file == "runbooks/no-fm.md"));
    let source_warnings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "sources")
        .collect();
    assert_eq!(source_warnings.len(), 2, "{source_warnings:#?}");
    assert!(source_warnings
        .iter()
        .any(|f| f.detail.contains(inline_missing)));
    assert!(source_warnings
        .iter()
        .any(|f| f.detail.contains(bare_missing)));
    assert!(!source_warnings
        .iter()
        .any(|f| f.detail.contains(present) || f.detail.contains(outside_sources)));
    assert!(report.links >= 4);
}

#[test]
fn preference_pool_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // under cap: no finding
    write(
        root,
        "preferences/small.md",
        "---\nname: small\ndescription: d\n---\nbody\n",
    );
    let report = validate::scan(root).unwrap();
    assert!(!report.findings.iter().any(|f| f.kind == "preference-pool"));
    // push the pool over 5120 bytes: error
    let big = format!(
        "---\nname: big\ndescription: d\n---\n{}\n",
        "x".repeat(5200)
    );
    write(root, "preferences/big.md", &big);
    let report = validate::scan(root).unwrap();
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == "preference-pool")
        .expect("oversize pool must flag");
    assert_eq!(f.severity, validate::Severity::Error);
    assert!(f.detail.contains("5120"));
}
