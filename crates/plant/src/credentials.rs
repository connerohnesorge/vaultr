//! The #1005 credential reconciler: projected Kubernetes secret -> credential
//! files in a computer's guest.
//!
//! Two things make this a running task rather than a plain volume mount:
//!
//!   1. Kubernetes never edits a projected file in place. It writes a new
//!      directory and re-points the `..data` symlink atomically, so a process
//!      holding an open FD to the old path reads stale content forever. We
//!      re-read by path on every pass and never cache a handle.
//!   2. Some consumers do not read the credential at all — they read a config
//!      derived from it. `39b9055` proved this in production: glab honoured a
//!      stale `is_oauth2: "true"` over the PAT it actually held, and every
//!      `glab api` call failed for 4.5h. Copying bytes is not enough; the
//!      derived config has to be rewritten too.
//!
//! Change detection polls a content hash. It does NOT use inotify, which is
//! unreliable across the projection swap — the same way allocator's
//! `systemd.path` watch never fired across virtiofs.
//!
//! Known limit, accepted in #1005: this does not restart running processes. A
//! `claude` session already started keeps the token it started with; the next
//! invocation picks up the new one.

use crate::fsutil::atomic_replace;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the control plane projects the owner's credential sets. #1049 owns
/// the real path; this default matches the directory the guest image creates.
pub const DEFAULT_SOURCE: &str = "/run/computers/credentials";

/// The manifest names every credential and the guest path it lands at. It is
/// data, not code, so #1049 can change the inventory without a plant release —
/// the same separation allocator already draws with its creds-Secret
/// `manifest.json`.
pub const MANIFEST: &str = "manifest.json";

#[derive(Debug, PartialEq, Eq)]
pub struct ReconcileArgs {
    /// Apply once and exit — the init-container mode. #1009 §5 chose one
    /// implementation over two that drift.
    pub once: bool,
    pub interval: Duration,
    pub source: PathBuf,
}

impl ReconcileArgs {
    pub fn with_defaults() -> Self {
        Self {
            once: false,
            interval: Duration::from_secs(30),
            source: std::env::var("COMPUTERS_CREDENTIAL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_SOURCE)),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Identifies the entry in logs. Not a path.
    pub name: String,
    /// Key within the projected directory holding the bytes.
    pub source: String,
    /// Absolute guest path the bytes land at.
    pub path: PathBuf,
    #[serde(default)]
    pub mode: Option<String>,
    /// Configs derived from this credential, rewritten whenever it changes.
    #[serde(default)]
    pub derive: Vec<Derived>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Derived {
    /// A `~/.git-credentials` line for one host.
    GitCredentials {
        path: PathBuf,
        host: String,
        #[serde(default = "oauth2_user")]
        user: String,
    },
    /// glab's `config.yml` host block: store the token and clear the stale
    /// OAuth metadata that outranks it.
    GlabConfig { path: PathBuf, host: String },
}

fn oauth2_user() -> String {
    "oauth2".to_string()
}

fn mode_of(entry: &Entry) -> u32 {
    entry
        .mode
        .as_deref()
        .and_then(|m| u32::from_str_radix(m.trim_start_matches("0o"), 8).ok())
        .unwrap_or(0o600)
}

pub fn load_manifest(source: &Path) -> Result<Manifest, String> {
    let path = source.join(MANIFEST);
    let bytes =
        std::fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

/// A digest over every source the manifest names, so a pass is skipped only
/// when nothing a credential depends on moved. A missing source hashes as
/// absent rather than erroring, so its later appearance still registers as a
/// change.
pub fn source_digest(source: &Path, manifest: &Manifest) -> String {
    let mut hasher = Sha256::new();
    for entry in &manifest.entries {
        hasher.update(entry.source.as_bytes());
        match std::fs::read(source.join(&entry.source)) {
            Ok(bytes) => {
                hasher.update([1u8]);
                hasher.update(bytes);
            }
            Err(_) => hasher.update([0u8]),
        }
    }
    format!("{:x}", hasher.finalize())
}

fn write_secret(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        // A 0600 file under a world-readable directory is still 0600, but the
        // directory listing leaks which credentials exist.
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    atomic_replace(path, bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))
}

/// Replace this host's line, leaving every other host's alone. git reads the
/// first matching line, so a stale duplicate would win over the fresh one.
pub fn merge_git_credentials(existing: &str, host: &str, user: &str, token: &str) -> String {
    let suffix = format!("@{host}");
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| !line.trim_end().ends_with(&suffix))
        .map(str::to_string)
        .collect();
    lines.push(format!("https://{user}:{token}@{host}"));
    lines.retain(|line| !line.trim().is_empty());
    format!("{}\n", lines.join("\n"))
}

/// Rewrite the `hosts: <host>:` block of glab's config.yml in place.
///
/// Deliberately line-level rather than a YAML round-trip: a full parse-and-emit
/// would drop the user's comments and reorder their keys, and the change we
/// need is three keys wide. `is_oauth2` is set to `false` rather than deleted
/// because glab treats an absent key as unset and an unset key has, in the
/// past, been re-inferred; `oauth2_expiry_date` is dropped outright since a
/// token with no expiry has no expiry date.
pub fn rewrite_glab_config(existing: &str, host: &str, token: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_hosts = false;
    let mut in_host = false;
    let mut host_indent = String::new();
    let mut emitted: BTreeMap<&str, bool> = [("token", false), ("is_oauth2", false)]
        .into_iter()
        .collect();

    let indent_of = |line: &str| line.len() - line.trim_start().len();

    for line in existing.lines() {
        let trimmed = line.trim();
        let indent = indent_of(line);

        if in_host && !trimmed.is_empty() && indent <= host_indent.len() {
            // Left the host block — flush anything the block did not carry.
            for (key, seen) in emitted.iter() {
                if !seen {
                    out.push(format!(
                        "{}  {}: {}",
                        host_indent,
                        key,
                        value_for(key, token)
                    ));
                }
            }
            in_host = false;
        }

        if trimmed == "hosts:" {
            in_hosts = true;
            out.push(line.to_string());
            continue;
        }

        if in_hosts && trimmed == format!("{host}:") {
            in_host = true;
            host_indent = " ".repeat(indent);
            out.push(line.to_string());
            continue;
        }

        if in_host {
            if let Some(key) = trimmed.split(':').next() {
                if key == "oauth2_expiry_date" || key == "oauth2_refresh_token" {
                    continue;
                }
                if let Some(seen) = emitted.get_mut(key) {
                    *seen = true;
                    out.push(format!(
                        "{}  {}: {}",
                        host_indent,
                        key,
                        value_for(key, token)
                    ));
                    continue;
                }
            }
        }

        out.push(line.to_string());
    }

    if in_host {
        for (key, seen) in emitted.iter() {
            if !seen {
                out.push(format!(
                    "{}  {}: {}",
                    host_indent,
                    key,
                    value_for(key, token)
                ));
            }
        }
    } else if !existing.lines().any(|l| l.trim() == format!("{host}:")) {
        // No block for this host at all — create the minimum glab needs.
        if !in_hosts {
            out.push("hosts:".to_string());
        }
        out.push(format!("  {host}:"));
        out.push(format!("    token: {token}"));
        out.push("    is_oauth2: false".to_string());
        out.push("    api_protocol: https".to_string());
        out.push(format!("    api_host: {host}"));
    }

    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn value_for(key: &str, token: &str) -> String {
    match key {
        "token" => token.to_string(),
        "is_oauth2" => "false".to_string(),
        _ => String::new(),
    }
}

fn apply_derived(derived: &Derived, token: &str) -> Result<(), String> {
    match derived {
        Derived::GitCredentials { path, host, user } => {
            let existing = std::fs::read_to_string(path).unwrap_or_default();
            let merged = merge_git_credentials(&existing, host, user, token);
            write_secret(path, merged.as_bytes(), 0o600)
        }
        Derived::GlabConfig { path, host } => {
            let existing = std::fs::read_to_string(path).unwrap_or_default();
            let rewritten = rewrite_glab_config(&existing, host, token);
            write_secret(path, rewritten.as_bytes(), 0o600)
        }
    }
}

/// One reconciliation pass. Returns the entries that failed, rather than
/// stopping at the first: one absent credential must not freeze the other
/// nine, which is the failure shape #1005 exists to prevent.
pub fn apply(source: &Path, manifest: &Manifest) -> Vec<String> {
    let mut failures = Vec::new();
    for entry in &manifest.entries {
        let from = source.join(&entry.source);
        let bytes = match std::fs::read(&from) {
            Ok(bytes) => bytes,
            Err(error) => {
                failures.push(format!(
                    "{}: cannot read {}: {error}",
                    entry.name,
                    from.display()
                ));
                continue;
            }
        };
        if let Err(error) = write_secret(&entry.path, &bytes, mode_of(entry)) {
            failures.push(format!("{}: {error}", entry.name));
            continue;
        }
        let token = String::from_utf8_lossy(&bytes).trim().to_string();
        for derived in &entry.derive {
            if let Err(error) = apply_derived(derived, &token) {
                failures.push(format!("{}: {error}", entry.name));
            }
        }
    }
    failures
}

fn report(failures: &[String]) {
    for failure in failures {
        eprintln!("[plant] credentials: {failure}");
    }
}

/// `--once`: one pass, exit non-zero if anything failed. Whether a vend should
/// hard-fail on that is the caller's call — the init container decides by
/// choosing to honour the exit code or not.
pub fn reconcile_once(source: &Path) -> i32 {
    let manifest = match load_manifest(source) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("[plant] credentials: {error}");
            return 1;
        }
    };
    let failures = apply(source, &manifest);
    report(&failures);
    if failures.is_empty() {
        println!(
            "[plant] credentials: reconciled {} entries",
            manifest.entries.len()
        );
        0
    } else {
        1
    }
}

/// Polling mode. Never exits: a supervised task that gives up on a transient
/// read error is a box that silently stops refreshing, which is the whole
/// thing #1005 is guarding against. It re-reads the manifest each pass so an
/// inventory change lands without a restart.
pub fn reconcile_loop(source: &Path, interval: Duration) -> ! {
    let mut last_digest = String::new();
    let mut last_error = String::new();
    loop {
        match load_manifest(source) {
            Ok(manifest) => {
                let digest = source_digest(source, &manifest);
                if digest != last_digest {
                    let failures = apply(source, &manifest);
                    report(&failures);
                    if failures.is_empty() {
                        println!(
                            "[plant] credentials: reconciled {} entries",
                            manifest.entries.len()
                        );
                        // Only a clean pass advances the digest, so a partial
                        // failure is retried instead of being latched as done.
                        last_digest = digest;
                        last_error.clear();
                    }
                }
            }
            Err(error) => {
                // Repeat-suppress: an absent manifest before the control plane
                // projects one should not fill the log at 2 lines a minute.
                if error != last_error {
                    eprintln!("[plant] credentials: {error}");
                    last_error = error;
                }
            }
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_credentials_replaces_only_the_named_host() {
        // The fixtures below are credential-shaped by construction — building
        // that exact line IS what is under test — so both scanners are told
        // per-line rather than by blanket-allowlisting this file.
        let existing = "https://oauth2:old@gitlab.com\nhttps://x:keep@github.com\n"; // pragma: allowlist secret gitleaks:allow
        let merged = merge_git_credentials(existing, "gitlab.com", "oauth2", "glpat-new");
        assert_eq!(
            merged,
            "https://x:keep@github.com\nhttps://oauth2:glpat-new@gitlab.com\n" // pragma: allowlist secret gitleaks:allow
        );
    }

    #[test]
    fn git_credentials_writes_a_first_line_into_an_empty_file() {
        assert_eq!(
            merge_git_credentials("", "gitlab.com", "oauth2", "glpat-new"),
            "https://oauth2:glpat-new@gitlab.com\n" // pragma: allowlist secret gitleaks:allow
        );
    }

    /// The 39b9055 shape: a stale is_oauth2 outranking the token actually held.
    #[test]
    fn glab_rewrite_clears_oauth2_and_drops_the_expiry() {
        let existing = "hosts:\n  gitlab.com:\n    token: old\n    is_oauth2: \"true\"\n    oauth2_expiry_date: 2026-08-05T06:53:00Z\n    api_protocol: https\n";
        let out = rewrite_glab_config(existing, "gitlab.com", "glpat-new");
        assert!(out.contains("    token: glpat-new"), "{out}");
        assert!(out.contains("    is_oauth2: false"), "{out}");
        assert!(!out.contains("oauth2_expiry_date"), "{out}");
        assert!(out.contains("    api_protocol: https"), "{out}");
    }

    #[test]
    fn glab_rewrite_leaves_other_hosts_untouched() {
        let existing =
            "hosts:\n  gitlab.com:\n    token: old\n  gitlab.example.com:\n    token: keepme\n    is_oauth2: \"true\"\n";
        let out = rewrite_glab_config(existing, "gitlab.com", "glpat-new");
        assert!(out.contains("token: glpat-new"), "{out}");
        assert!(out.contains("token: keepme"), "{out}");
        assert!(out.contains("is_oauth2: \"true\""), "{out}");
    }

    #[test]
    fn glab_rewrite_adds_is_oauth2_when_the_block_omits_it() {
        let existing = "hosts:\n  gitlab.com:\n    token: old\n";
        let out = rewrite_glab_config(existing, "gitlab.com", "glpat-new");
        assert!(out.contains("    is_oauth2: false"), "{out}");
    }

    #[test]
    fn glab_rewrite_creates_a_config_from_nothing() {
        let out = rewrite_glab_config("", "gitlab.com", "glpat-new");
        assert!(out.starts_with("hosts:\n"), "{out}");
        assert!(out.contains("  gitlab.com:"), "{out}");
        assert!(out.contains("    token: glpat-new"), "{out}");
        assert!(out.contains("    is_oauth2: false"), "{out}");
    }

    #[test]
    fn manifest_parses_the_documented_shape() {
        let json = r#"{"entries":[
            {"name":"claude","source":"claude.json","path":"/home/dev/.claude/.credentials.json"},
            {"name":"glab","source":"gitlab-pat","path":"/home/dev/.config/glab/token","mode":"0600",
             "derive":[{"kind":"git-credentials","path":"/home/dev/.git-credentials","host":"gitlab.com"},
                       {"kind":"glab-config","path":"/home/dev/.config/glab/config.yml","host":"gitlab.com"}]}
        ]}"#;
        let manifest: Manifest = serde_json::from_str(json).expect("parses");
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(manifest.entries[0].derive.len(), 0);
        assert_eq!(mode_of(&manifest.entries[0]), 0o600);
        assert_eq!(
            manifest.entries[1].derive[0],
            Derived::GitCredentials {
                path: PathBuf::from("/home/dev/.git-credentials"),
                host: "gitlab.com".to_string(),
                user: "oauth2".to_string(),
            }
        );
    }

    #[test]
    fn digest_moves_when_a_source_changes_and_when_one_appears() {
        let dir = std::env::temp_dir().join(format!("plant-cred-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let manifest = Manifest {
            entries: vec![
                Entry {
                    name: "a".into(),
                    source: "a".into(),
                    path: dir.join("out-a"),
                    mode: None,
                    derive: vec![],
                },
                Entry {
                    name: "b".into(),
                    source: "b".into(),
                    path: dir.join("out-b"),
                    mode: None,
                    derive: vec![],
                },
            ],
        };

        std::fs::write(dir.join("a"), b"one").expect("write");
        let first = source_digest(&dir, &manifest);

        std::fs::write(dir.join("a"), b"two").expect("write");
        let changed = source_digest(&dir, &manifest);
        assert_ne!(first, changed, "a changed source must move the digest");

        std::fs::write(dir.join("b"), b"appeared").expect("write");
        let appeared = source_digest(&dir, &manifest);
        assert_ne!(
            changed, appeared,
            "a newly present source must move the digest"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_reports_the_missing_entry_and_still_writes_the_present_one() {
        let dir = std::env::temp_dir().join(format!("plant-cred-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("present"), b"glpat-live").expect("write");
        let manifest = Manifest {
            entries: vec![
                Entry {
                    name: "absent".into(),
                    source: "absent".into(),
                    path: dir.join("out-absent"),
                    mode: None,
                    derive: vec![],
                },
                Entry {
                    name: "present".into(),
                    source: "present".into(),
                    path: dir.join("out-present"),
                    mode: None,
                    derive: vec![Derived::GlabConfig {
                        path: dir.join("config.yml"),
                        host: "gitlab.com".into(),
                    }],
                },
            ],
        };

        let failures = apply(&dir, &manifest);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("absent:"), "{failures:?}");

        let written = std::fs::read(dir.join("out-present")).expect("written");
        assert_eq!(written, b"glpat-live");
        let mode = std::fs::metadata(dir.join("out-present"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a credential must not be group- or world-readable"
        );

        let config = std::fs::read_to_string(dir.join("config.yml")).expect("derived");
        assert!(config.contains("token: glpat-live"), "{config}");
        assert!(config.contains("is_oauth2: false"), "{config}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
