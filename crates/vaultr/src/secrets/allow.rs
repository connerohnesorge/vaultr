// Scanner policy loading and path/digest-scoped review decisions.

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

const SECRETS_SECTION: &str = "[secrets]";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AllowEntry {
    pub(crate) path: String,
    pub(crate) digest: String,
    pub(crate) rule: String,
    pub(crate) note: String,
    pub(crate) added: String,
}

#[derive(Default, Deserialize, Serialize)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<AllowEntry>,
}

pub(crate) struct LoadedPolicy {
    pub(crate) ignored_paths: Gitignore,
    pub(crate) ignored_secrets: HashSet<Vec<u8>>,
    pub(crate) allows: Vec<AllowEntry>,
}

pub(crate) fn load(repo_root: &Path) -> Result<LoadedPolicy> {
    let ignore_path = repo_root.join(".secretsignore");
    let mut builder = GitignoreBuilder::new(repo_root);
    let mut ignored_secrets = HashSet::new();
    if ignore_path.exists() {
        let contents = std::fs::read_to_string(&ignore_path)
            .with_context(|| format!("read {}", ignore_path.display()))?;
        let mut in_secrets = false;
        for line in contents.lines() {
            if line.trim() == SECRETS_SECTION {
                in_secrets = true;
                continue;
            }
            if in_secrets {
                let secret = line.trim_end();
                if !secret.is_empty() && !secret.starts_with('#') {
                    ignored_secrets.insert(secret.as_bytes().to_vec());
                }
            } else {
                builder
                    .add_line(Some(ignore_path.clone()), line)
                    .map_err(|error| anyhow::anyhow!("parse {}: {error}", ignore_path.display()))?;
            }
        }
    }
    let ignored_paths = builder
        .build()
        .map_err(|error| anyhow::anyhow!("build .secretsignore: {error}"))?;

    let allow_path = repo_root.join(".secrets-allow.toml");
    let allows = if allow_path.exists() {
        let contents = std::fs::read_to_string(&allow_path)
            .with_context(|| format!("read {}", allow_path.display()))?;
        toml::from_str::<AllowFile>(&contents)
            .with_context(|| format!("parse {}", allow_path.display()))?
            .allow
    } else {
        Vec::new()
    };

    Ok(LoadedPolicy {
        ignored_paths,
        ignored_secrets,
        allows,
    })
}

pub(crate) fn relative_string(path: &Path) -> Option<String> {
    if path.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return None;
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

pub(crate) fn is_ignored(paths: &Gitignore, rel_path: &Path) -> bool {
    paths
        .matched_path_or_any_parents(rel_path, false)
        .is_ignore()
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn is_allowed(
    allows: &[AllowEntry],
    rel_path: &Path,
    rule: &str,
    matched: &[u8],
) -> bool {
    let Some(path) = relative_string(rel_path) else {
        return false;
    };
    let wanted_digest = digest(matched);
    allows
        .iter()
        .any(|entry| entry.path == path && entry.digest == wanted_digest && entry.rule == rule)
}

pub(crate) fn append_allow(repo_root: &Path, entry: &AllowEntry) -> Result<()> {
    let path = repo_root.join(".secrets-allow.toml");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let existing_len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    use std::io::Write;
    if existing_len > 0 {
        file.write_all(b"\n")?;
    }
    let body = toml::to_string_pretty(&AllowFile {
        allow: vec![entry.clone()],
    })?;
    file.write_all(body.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_path_globs_and_literal_secrets_separately() {
        let root = tempdir().unwrap();
        std::fs::write(
            root.path().join(".secretsignore"),
            "mail/\n*.generated\n[secrets]\nknown-secret\n# comment\n",
        )
        .unwrap();
        let policy = load(root.path()).unwrap();
        assert!(is_ignored(
            &policy.ignored_paths,
            Path::new("mail/body.json")
        ));
        assert!(is_ignored(&policy.ignored_paths, Path::new("x.generated")));
        assert!(policy.ignored_secrets.contains(b"known-secret".as_slice()));
        assert!(!policy.ignored_secrets.contains(b"# comment".as_slice()));
    }

    #[test]
    fn allowlist_is_path_and_digest_scoped() {
        let entry = AllowEntry {
            path: "one.txt".into(),
            digest: digest(b"same-token"),
            rule: "random-string".into(),
            note: "fixture".into(),
            added: "2026-08-06".into(),
        };
        assert!(is_allowed(
            std::slice::from_ref(&entry),
            Path::new("one.txt"),
            "random-string",
            b"same-token"
        ));
        assert!(!is_allowed(
            std::slice::from_ref(&entry),
            Path::new("two.txt"),
            "random-string",
            b"same-token"
        ));
    }
}
