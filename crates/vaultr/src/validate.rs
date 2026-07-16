//! Strict vault content validation: [[wikilink]] resolution, frontmatter schema,
//! markdown path links, and learn-ledger integrity. Read-only over the vault.
//!
//! Severity model: broken links / bad md paths / unparseable ledger lines / duplicate
//! slugs (bare-[[slug]] links become ambiguous) / an oversize preference pool are
//! errors; frontmatter gaps are warnings — 87 legacy files have no frontmatter and
//! must not trip the repair loop.
//! A line containing `<!-- vault-validate: ignore -->` is exempt from link checks.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const CONTENT_DIRS: &[&str] = &[
    "learnings",
    "preferences",
    "decisions",
    "runbooks",
    "people",
    "systems",
    "projects",
    "glossary",
    "incidents",
    "conversations",
    "digests",
];

const IGNORE_MARKER: &str = "<!-- vault-validate: ignore -->";

/// preferences/*.md are inlined verbatim into every session's digest — hard cap.
const PREF_POOL_CAP: u64 = 5120;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub kind: &'static str,
    /// Path relative to the content root.
    pub file: String,
    pub line: usize,
    pub detail: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub files: usize,
    pub links: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count()
    }
    pub fn warnings(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count()
    }
    pub fn summary(&self) -> String {
        format!(
            "{} files, {} links, {} errors, {} warnings",
            self.files,
            self.links,
            self.errors(),
            self.warnings()
        )
    }
}

/// Content root is the parent of the sessions root (~/.dotfiles/vault).
pub fn content_root(sessions_root: &Path) -> Result<PathBuf> {
    sessions_root
        .parent()
        .map(Path::to_path_buf)
        .context("sessions root has no parent")
}

/// Learn-ledger path under the content root: learnings/.ledger.jsonl.
pub fn ledger_path(content_root: &Path) -> PathBuf {
    content_root.join("learnings/.ledger.jsonl")
}

/// Required frontmatter keys per content dir (warnings when missing).
fn required_keys(dir: &str) -> &'static [&'static str] {
    match dir {
        "learnings" => &["name", "description", "type"],
        "people" | "projects" | "systems" => &["type", "title"],
        _ => &[],
    }
}

fn md_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Drop inline `code` spans: split on backticks, keep even-index segments.
fn strip_inline_code(line: &str) -> String {
    line.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .map(|(_, s)| s)
        .collect()
}

/// Frontmatter keys (top-level `key:` lines between leading `---` fences),
/// plus the frontmatter's raw lines for sources extraction. None => no frontmatter.
fn frontmatter(text: &str) -> Option<(HashSet<String>, Vec<String>)> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut keys = HashSet::new();
    let mut raw = Vec::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some((keys, raw));
        }
        if !line.starts_with([' ', '\t', '-']) {
            if let Some((k, _)) = line.split_once(':') {
                keys.insert(k.trim().to_string());
            }
        }
        raw.push(line.to_string());
    }
    None // unterminated fence => treat as no frontmatter
}

/// Wikilink targets in a (code-stripped) line. Aliases/anchors split off.
fn wikilink_targets(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let raw = &after[..end];
        rest = &after[end + 2..];
        let target = raw.split(['#', '|']).next().unwrap_or("").trim();
        // bash test / character-class literals that survive code stripping
        if target.is_empty()
            || target
                .chars()
                .any(|c| "$(){}:<>\"'`!=&;".contains(c) || c == '[' || c == ']')
        {
            continue;
        }
        out.push(target.to_string());
    }
    out
}

/// Markdown path-link targets like `](/dir/file.md)` in a (code-stripped) line.
fn mdpath_targets(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("](/") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else { break };
        let target = &after[..end];
        rest = &after[end + 1..];
        if target.ends_with(".md") && !target.contains(' ') {
            out.push(target.to_string());
        }
    }
    out
}

pub fn scan(content_root: &Path) -> Result<Report> {
    let mut report = Report::default();
    // Pass 1: slug index (filename stems across all content dirs + vault root).
    let mut slugs: HashSet<String> = HashSet::new();
    let mut slug_dirs: HashMap<String, Vec<String>> = HashMap::new();
    let mut all: Vec<(String, PathBuf, &str)> = Vec::new(); // (rel, path, dir)
    for dir in CONTENT_DIRS {
        for path in md_files(&content_root.join(dir)) {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            slugs.insert(stem.to_string());
            slug_dirs
                .entry(stem.to_string())
                .or_default()
                .push(dir.to_string());
            all.push((format!("{dir}/{}", path.file_name().unwrap().to_string_lossy()), path, dir));
        }
    }
    for path in md_files(content_root) {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        slugs.insert(stem.to_string());
    }
    report.files = all.len();

    for (slug, dirs) in &slug_dirs {
        // digests are auto-generated per-project snapshots — sharing the project's slug
        // (and lacking frontmatter) is by design, not drift
        let dirs: Vec<String> = dirs.iter().filter(|d| *d != "digests").cloned().collect();
        if dirs.len() > 1 {
            report.findings.push(Finding {
                severity: Severity::Error,
                kind: "duplicate-slug",
                file: format!("{}/{slug}.md", dirs[0]),
                line: 0,
                detail: format!("slug '{slug}' exists in: {}", dirs.join(", ")),
            });
        }
    }

    // Pass 2: per-file checks.
    for (rel, path, dir) in &all {
        let Ok(text) = std::fs::read_to_string(path) else {
            report.findings.push(Finding {
                severity: Severity::Error,
                kind: "unreadable",
                file: rel.clone(),
                line: 0,
                detail: "file is not readable UTF-8".into(),
            });
            continue;
        };
        match frontmatter(&text) {
            None if *dir == "digests" => {}
            None => report.findings.push(Finding {
                severity: Severity::Warning,
                kind: "frontmatter",
                file: rel.clone(),
                line: 1,
                detail: "missing frontmatter block".into(),
            }),
            Some((keys, _)) => {
                for k in required_keys(dir) {
                    if !keys.contains(*k) {
                        report.findings.push(Finding {
                            severity: Severity::Warning,
                            kind: "frontmatter",
                            file: rel.clone(),
                            line: 1,
                            detail: format!("missing required key '{k}'"),
                        });
                    }
                }
            }
        }
        // link-scan the body only — frontmatter descriptions quote [[wikilinks]] as prose
        let mut in_fence = false;
        let mut in_frontmatter = false;
        for (i, line) in text.lines().enumerate() {
            if i == 0 && line.trim_end() == "---" {
                in_frontmatter = true;
                continue;
            }
            if in_frontmatter {
                if line.trim_end() == "---" {
                    in_frontmatter = false;
                }
                continue;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence || line.contains(IGNORE_MARKER) {
                continue;
            }
            let clean = strip_inline_code(line);
            for target in wikilink_targets(&clean) {
                report.links += 1;
                // [[dir/slug]] path-style links resolve against the content root
                let resolved = if target.contains('/') {
                    content_root.join(format!("{target}.md")).is_file()
                } else {
                    slugs.contains(&target)
                };
                if !resolved {
                    report.findings.push(Finding {
                        severity: Severity::Error,
                        kind: "wikilink",
                        file: rel.clone(),
                        line: i + 1,
                        detail: format!("[[{target}]] does not resolve to any note"),
                    });
                }
            }
            for target in mdpath_targets(&clean) {
                report.links += 1;
                if !content_root.join(target.trim_start_matches('/')).is_file() {
                    report.findings.push(Finding {
                        severity: Severity::Error,
                        kind: "mdpath",
                        file: rel.clone(),
                        line: i + 1,
                        detail: format!("]({target}) target does not exist"),
                    });
                }
            }
        }
    }

    // Ledger integrity: every non-empty line parses as JSON with session_id.
    let ledger = ledger_path(content_root);
    if let Ok(text) = std::fs::read_to_string(&ledger) {
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let ok = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_some_and(|v| v.get("session_id").and_then(|s| s.as_str()).is_some());
            if !ok {
                report.findings.push(Finding {
                    severity: Severity::Error,
                    kind: "ledger",
                    file: "learnings/.ledger.jsonl".into(),
                    line: i + 1,
                    detail: "line is not JSON with a session_id".into(),
                });
            }
        }
    }

    // Preference pool cap: the pool rides in every session, so oversize is an error
    // the repair loop must consolidate (merge/shorten/delete), never silently drop.
    let pref_bytes: u64 = md_files(&content_root.join("preferences"))
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if pref_bytes > PREF_POOL_CAP {
        report.findings.push(Finding {
            severity: Severity::Error,
            kind: "preference-pool",
            file: "preferences".into(),
            line: 0,
            detail: format!(
                "pool is {pref_bytes}B, cap {PREF_POOL_CAP}B — consolidate: merge overlapping, shorten verbose, delete stale; never drop a preference silently"
            ),
        });
    }

    // sources: frontmatter session-UUID refs must exist in the sessions index.
    let meta_dir = content_root.join("sessions/.meta");
    for (rel, path, _) in &all {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some((_, raw)) = frontmatter(&text) else {
            continue;
        };
        let mut in_sources = false;
        for line in &raw {
            if !line.starts_with([' ', '\t', '-']) {
                in_sources = line.split(':').next().map(str::trim) == Some("sources");
                continue;
            }
            if !in_sources {
                continue;
            }
            let item = line.trim().trim_start_matches('-').trim();
            if looks_like_uuid(item) && !meta_dir.join(format!("{item}.json")).is_file() {
                report.findings.push(Finding {
                    severity: Severity::Warning,
                    kind: "sources",
                    file: rel.clone(),
                    line: 0,
                    detail: format!("source session {item} not in sessions/.meta"),
                });
            }
        }
    }

    report.findings.sort_by(|a, b| {
        (a.severity != Severity::Error, &a.file, a.line).cmp(&(
            b.severity != Severity::Error,
            &b.file,
            b.line,
        ))
    });
    Ok(report)
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars()
            .enumerate()
            .all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            })
}

/// CLI entry: print report, return process exit code.
pub fn run(sessions_root: &Path, json: bool, strict: bool) -> Result<i32> {
    let root = content_root(sessions_root)?;
    let report = scan(&root)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for f in &report.findings {
            println!(
                "{}: {} {}:{} {}",
                match f.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                f.kind,
                f.file,
                f.line,
                f.detail
            );
        }
        println!("{}", report.summary());
    }
    let fail = report.errors() > 0 || (strict && report.warnings() > 0);
    Ok(if fail { 1 } else { 0 })
}
