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
    assert!(errs.iter().any(|f| f.kind == "wikilink" && f.detail.contains("does-not-exist")));
    assert!(errs.iter().any(|f| f.kind == "mdpath" && f.detail.contains("/learnings/nope.md")));
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
    let big = format!("---\nname: big\ndescription: d\n---\n{}\n", "x".repeat(5200));
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
