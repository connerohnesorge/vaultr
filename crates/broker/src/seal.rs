//! What counts as a seal key.
//!
//! The broker holds a write grant into a bucket of regulated agent-transcript
//! data, so the key it is handed decides what that grant can be talked into
//! writing. Validation is therefore a whitelist of the one shape a seal has —
//! `sessions/YYYY/MM/DD/<session-id>/<seal file>` — rather than a blacklist of
//! traversal tricks.
//!
//! The set of seal *files* is a named, configurable choice rather than a string
//! buried in a matcher. Today it is `turns.jsonl.zst` alone: keeping `herdr.jsonl.zst`
//! in git is a decision taken twice, deliberately, and this service does not
//! revisit it. But that decision has an open ticket against it, and when it
//! flips this must be a config change and not an archaeology exercise.

use anyhow::{bail, Result};

/// The seal types this broker accepts. Override with `SEAL_BROKER_SEAL_FILES`
/// (comma-separated) when the herdr question is decided.
pub const DEFAULT_SEAL_FILES: &[&str] = &["turns.jsonl.zst"];

/// The key layout, split for validation: `sessions/<year>/<month>/<day>/<id>/<file>`.
const SEGMENTS: usize = 6;

pub struct KeyPolicy {
    seal_files: Vec<String>,
}

impl KeyPolicy {
    pub fn new(seal_files: Vec<String>) -> Self {
        KeyPolicy { seal_files }
    }

    pub fn seal_files(&self) -> &[String] {
        &self.seal_files
    }

    /// Accept a vault-relative seal key, or say precisely what is wrong with it.
    pub fn validate(&self, key: &str) -> Result<()> {
        if key.is_empty() {
            bail!("empty seal key");
        }
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != SEGMENTS {
            bail!(
                "seal key must be sessions/YYYY/MM/DD/<session-id>/<seal file>, \
                 got {} segment(s) in {key:?}",
                parts.len()
            );
        }
        if parts[0] != "sessions" {
            bail!("seal key must start with sessions/, got {key:?}");
        }
        for (part, width) in [(parts[1], 4), (parts[2], 2), (parts[3], 2)] {
            if part.len() != width || !part.bytes().all(|b| b.is_ascii_digit()) {
                bail!("seal key carries a non-date path segment {part:?} in {key:?}");
            }
        }
        let id = parts[4];
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("seal key carries an unusable session id {id:?} in {key:?}");
        }
        let file = parts[5];
        if !self.seal_files.iter().any(|allowed| allowed == file) {
            bail!(
                "{file:?} is not a seal this broker stores (accepts: {})",
                self.seal_files.join(", ")
            );
        }
        Ok(())
    }
}

impl Default for KeyPolicy {
    fn default() -> Self {
        KeyPolicy::new(DEFAULT_SEAL_FILES.iter().map(|s| s.to_string()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_seal_key_is_accepted() {
        let policy = KeyPolicy::default();
        policy
            .validate("sessions/2026/08/03/d01211c2-e6a7-40e6-8db3-164f24232f60/turns.jsonl.zst")
            .unwrap();
    }

    // The grant this service holds is write-into-a-regulated-bucket, so every
    // way of pointing it somewhere else has to be a refusal, not a sanitisation.
    #[test]
    fn nothing_outside_the_seal_layout_is_writable() {
        let policy = KeyPolicy::default();
        for key in [
            "",
            "turns.jsonl.zst",
            "sessions/2026/08/03/abc/turns.jsonl.zst/extra",
            "../../etc/passwd",
            "sessions/../../etc/passwd",
            "sessions/2026/08/03/../evil/turns.jsonl.zst",
            "/sessions/2026/08/03/abc/turns.jsonl.zst",
            "learnings/2026/08/03/abc/turns.jsonl.zst",
            "sessions/26/08/03/abc/turns.jsonl.zst",
            "sessions/2026/8/03/abc/turns.jsonl.zst",
            "sessions/2026/08/03//turns.jsonl.zst",
            "sessions/2026/08/03/abc/.meta.json",
        ] {
            assert!(policy.validate(key).is_err(), "accepted {key:?}");
        }
    }

    // herdr sidecars are still in git on purpose; the broker declines them today
    // and the decision to include them is one config value, not a code hunt.
    #[test]
    fn the_seal_type_is_configuration_not_a_buried_string() {
        let policy = KeyPolicy::default();
        assert!(policy
            .validate("sessions/2026/08/03/abc/herdr.jsonl.zst")
            .is_err());
        let widened = KeyPolicy::new(vec!["turns.jsonl.zst".into(), "herdr.jsonl.zst".into()]);
        widened
            .validate("sessions/2026/08/03/abc/herdr.jsonl.zst")
            .unwrap();
    }
}
