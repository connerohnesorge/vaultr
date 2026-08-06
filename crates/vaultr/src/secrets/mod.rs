// Native, byte-oriented secret detection shared by vaultr and Plant.

mod allow;
mod matcher;
mod p_random;
mod patterns;

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use allow::{AllowEntry, LoadedPolicy};
use matcher::{CompiledPattern, Detected};

/// Scan policy loaded from a repository root.
pub struct Policy {
    ignored_paths: Gitignore,
    ignored_secrets: HashSet<Vec<u8>>,
    allows: Vec<AllowEntry>,
    patterns: Vec<CompiledPattern>,
}

/// A secret finding. `span` is a byte range in the scanned input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: &'static str,
    pub line: usize,
    pub col: usize,
    pub span: Range<usize>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ignored_paths: Gitignore::empty(),
            ignored_secrets: HashSet::new(),
            allows: Vec::new(),
            patterns: compile_patterns().expect("built-in secret patterns are valid"),
        }
    }
}

impl Policy {
    pub(crate) fn allows(&self, rel_path: &Path, detected: &Detected, bytes: &[u8]) -> bool {
        allow::is_allowed(
            &self.allows,
            rel_path,
            detected.rule,
            &bytes[detected.span.clone()],
        )
    }

    pub(crate) fn ignored_path(&self, rel_path: &Path) -> bool {
        allow::is_ignored(&self.ignored_paths, rel_path)
    }

    pub(crate) fn is_policy_file(rel_path: &Path) -> bool {
        matches!(
            rel_path.to_str(),
            Some(".secretsignore") | Some(".secrets-allow.toml")
        )
    }

    pub(crate) fn patterns(&self) -> &[CompiledPattern] {
        &self.patterns
    }

    pub(crate) fn ignored_secrets(&self) -> &HashSet<Vec<u8>> {
        &self.ignored_secrets
    }

    pub(crate) fn add_allow(&mut self, entry: AllowEntry) {
        self.allows.push(entry);
    }
}

fn compile_patterns() -> Result<Vec<CompiledPattern>> {
    let random = patterns::PatternSpec {
        id: "random-string",
        expression: matcher::RANDOM_STRING_REGEX,
        secret_group: Some(1),
    };
    patterns::RIPSECRETS_PATTERNS
        .iter()
        .chain(patterns::PLANT_PATTERNS.iter())
        .copied()
        .chain(std::iter::once(random))
        .map(matcher::compile)
        .collect::<Result<Vec<_>, _>>()
        .context("compile built-in secret patterns")
}

/// Load `.secretsignore` and `.secrets-allow.toml` from a repository root.
pub fn policy_for(repo_root: &Path) -> Result<Policy> {
    let root = repo_root
        .canonicalize()
        .with_context(|| format!("resolve repository root {}", repo_root.display()))?;
    let LoadedPolicy {
        ignored_paths,
        ignored_secrets,
        allows,
    } = allow::load(&root)?;
    Ok(Policy {
        ignored_paths,
        ignored_secrets,
        allows,
        patterns: compile_patterns()?,
    })
}

fn line_col(bytes: &[u8], offset: usize) -> (usize, usize) {
    let before = &bytes[..offset.min(bytes.len())];
    let line = before.iter().filter(|byte| **byte == b'\n').count() + 1;
    let col = before
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(before.len(), |newline| before.len() - newline - 1)
        + 1;
    (line, col)
}

fn text_path(rel_path: &Path) -> bool {
    rel_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("zst")
}

fn text_bytes(bytes: &[u8]) -> bool {
    // NUL is the conservative binary marker. The matcher itself is byte based
    // so valid non-UTF-8 text remains scannable.
    !bytes.contains(&0)
}

fn detected_hit(bytes: &[u8], detected: Detected) -> Hit {
    let (line, col) = line_col(bytes, detected.span.start);
    Hit {
        rule: detected.rule,
        line,
        col,
        span: detected.span,
    }
}

/// Scan one repository-relative byte stream.
pub fn scan_bytes(bytes: &[u8], rel_path: &Path, policy: &Policy) -> Vec<Hit> {
    if !text_path(rel_path)
        || !text_bytes(bytes)
        || Policy::is_policy_file(rel_path)
        || policy.ignored_path(rel_path)
    {
        return Vec::new();
    }
    matcher::find(bytes, policy.patterns(), policy.ignored_secrets())
        .into_iter()
        .filter(|detected| !policy.allows(rel_path, detected, bytes))
        .map(|detected| detected_hit(bytes, detected))
        .collect()
}

/// Redact one text line using the same pattern set as the scanner.
pub fn redact_line(line: &str, policy: &Policy) -> (String, usize) {
    let bytes = line.as_bytes();
    let detections = matcher::find(bytes, policy.patterns(), policy.ignored_secrets());
    let mut redactions: Vec<_> = detections
        .iter()
        .map(|detected| detected.span.clone())
        .collect();
    redactions.sort_by_key(|range| std::cmp::Reverse(range.start));
    let mut output = line.to_owned();
    for range in redactions {
        if output.is_char_boundary(range.start) && output.is_char_boundary(range.end) {
            output.replace_range(range, "[REDACTED]");
        }
    }
    (output, detections.len())
}

/// Append a path/digest-scoped allowlist decision and update a loaded policy.
pub(crate) fn allow_false_positive(
    repo_root: &Path,
    rel_path: &Path,
    rule: &'static str,
    matched: &[u8],
    note: &str,
    added: &str,
    policy: &mut Policy,
) -> Result<()> {
    let path = allow::relative_string(rel_path)
        .map(PathBuf::from)
        .context("allowlist path must be repository-relative")?;
    let entry = AllowEntry {
        path: path.to_string_lossy().into_owned(),
        digest: allow::digest(matched),
        rule: rule.to_string(),
        note: note.to_string(),
        added: added.to_string(),
    };
    allow::append_allow(repo_root, &entry)?;
    policy.add_allow(entry);
    Ok(())
}

pub(crate) fn digest_for_review(bytes: &[u8]) -> String {
    allow::digest(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detects_provider_and_plant_rules_without_echoing_values() {
        let policy = Policy::default();
        let bytes = b"GITLAB_TOKEN=glpat-3Kd9Vq2ZmXr7Lb1TnWpA\naws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
        let hits = scan_bytes(bytes, Path::new("leak.env"), &policy);
        assert!(hits
            .iter()
            .any(|hit| { matches!(hit.rule, "gitlab-token" | "gitlab-pat" | "random-string") }));
        assert!(hits.iter().any(|hit| hit.rule == "aws-secret-access-key"));
        let rendered = hits
            .iter()
            .map(|hit| format!("{}:{}:{}", hit.rule, hit.line, hit.col))
            .collect::<String>();
        assert!(!rendered.contains("3Kd9Vq2"));
    }

    #[test]
    fn keeps_the_sk_delimiter_guard_and_skips_json_escape_false_positive() {
        let policy = Policy::default();
        let base64 = b"{\"blob\":\"aGVsbG9Xb3JsZHNrLQABsk-Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA\"}";
        assert!(scan_bytes(base64, Path::new("capture.json"), &policy).is_empty());

        // Regression for KIRA_SF_KEY= followed by literal JSON escapes in a
        // serialized capture. This used to satisfy RANDOM_STRING_REGEX.
        let escaped = b"KIRA_SF_KEY=\\n0123456789abcdef\\nset";
        assert!(scan_bytes(escaped, Path::new("capture.json"), &policy).is_empty());
    }

    #[test]
    fn pragma_and_binary_and_zst_inputs_are_skipped() {
        let policy = Policy::default();
        let token = b"GITLAB_TOKEN=glpat-3Kd9Vq2ZmXr7Lb1TnWpA";
        assert!(scan_bytes(
            b"GITLAB_TOKEN=glpat-3Kd9Vq2ZmXr7Lb1TnWpA # pragma: allowlist secret",
            Path::new("allowed.env"),
            &policy
        )
        .is_empty());
        assert!(scan_bytes(token, Path::new("capture.zst"), &policy).is_empty());
        assert!(scan_bytes(
            &[token.as_slice(), b"\0"].concat(),
            Path::new("capture.bin"),
            &policy
        )
        .is_empty());
    }

    #[test]
    fn policy_allowlist_does_not_cross_paths() {
        let root = tempdir().unwrap();
        let digest = allow::digest(b"glpat-3Kd9Vq2ZmXr7Lb1TnWpA");
        std::fs::write(
            root.path().join(".secrets-allow.toml"),
            format!(
                "[[allow]]\npath=\"one.env\"\ndigest=\"{digest}\"\nrule=\"random-string\"\nnote=\"fixture\"\nadded=\"2026-08-06\"\n"
            ),
        )
        .unwrap();
        let policy = policy_for(root.path()).unwrap();
        let bytes = b"GITLAB_TOKEN=glpat-3Kd9Vq2ZmXr7Lb1TnWpA";
        assert!(scan_bytes(bytes, Path::new("one.env"), &policy).is_empty());
        assert!(!scan_bytes(bytes, Path::new("two.env"), &policy).is_empty());
    }
}
