//! The seal store, reached through the `aws` CLI.
//!
//! Transport matches `vaultr::seals` — the read side of the same bucket — and
//! for the same reason it gives there: the hard part of reaching this store is
//! credential resolution, not the request. The broker resolves IRSA in athens,
//! SSO on the Mac while it is being proven locally, and environment variables in
//! CI, and the CLI already resolves all three. Nothing here is hot enough for
//! process overhead to matter: one listing per client reconcile, one existence
//! check and one upload per changed seal.
//!
//! Two calls only, and both are chosen to fit the write-only grant this service
//! holds (`s3:ListBucket` on `sessions/*` + `s3:PutObject` on `sessions/*`, no
//! `s3:GetObject`):
//!
//! - `list-objects-v2` returns key *and* size, which is everything an idempotent
//!   size-checked comparison needs. `head-object` would have been the obvious
//!   call and is the wrong one — it is authorised by `s3:GetObject`, which this
//!   service must not hold.
//! - `put-object` is single-part. `aws s3 cp` switches to multipart above 8 MB,
//!   which drags in `s3:AbortMultipartUpload` to clean up after a failure;
//!   single-part keeps the IAM policy exactly as narrow as it was written, at
//!   the cost of a 5 GiB per-object ceiling that the largest seal on record
//!   (2.7 GB) sits comfortably under.

use anyhow::{bail, Context, Result};
use std::path::Path;
use tokio::process::Command;

/// The store of record. Keys are the vault-relative path of the seal:
/// `sessions/YYYY/MM/DD/<session-id>/turns.jsonl.zst`, so a local path and its
/// S3 key are the same string and no mapping exists to get wrong.
pub const DEFAULT_BUCKET: &str = "pantheon-vault-seals-athens";

/// Every key the broker will ever touch sits under this prefix. Listing is
/// scoped to it so a bucket that later holds anything else stays invisible here.
pub const KEY_PREFIX: &str = "sessions/";

/// S3's ceiling for a single-part `PutObject`.
pub const MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Store {
    bucket: String,
}

/// One object as the store reports it: the two fields idempotence needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
}

impl Store {
    pub fn new(bucket: impl Into<String>) -> Self {
        Store {
            bucket: bucket.into(),
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Every seal in the store, as key and size.
    ///
    /// The CLI paginates this itself, so ~9.5k objects arrive as one response.
    /// `--output text` with a two-field projection keeps that a tab-separated
    /// stream rather than a megabyte and a half of JSON to parse.
    pub async fn list(&self) -> Result<Vec<Object>> {
        let stdout = self
            .aws(&[
                "s3api",
                "list-objects-v2",
                "--bucket",
                &self.bucket,
                "--prefix",
                KEY_PREFIX,
                "--query",
                "Contents[].[Key,Size]",
                "--output",
                "text",
            ])
            .await?;
        Ok(parse_listing(&stdout))
    }

    /// The size the store holds for one key, or `None` when it holds nothing.
    ///
    /// A prefix search can match a longer key that merely starts with this one,
    /// so the result is filtered for an exact match rather than trusted.
    pub async fn size_of(&self, key: &str) -> Result<Option<u64>> {
        let stdout = self
            .aws(&[
                "s3api",
                "list-objects-v2",
                "--bucket",
                &self.bucket,
                "--prefix",
                key,
                "--query",
                "Contents[].[Key,Size]",
                "--output",
                "text",
            ])
            .await?;
        Ok(parse_listing(&stdout)
            .into_iter()
            .find(|object| object.key == key)
            .map(|object| object.size))
    }

    /// Write one object. The body is a file so the transfer streams and the
    /// length is exact; the caller has already spooled and size-checked it.
    pub async fn put(&self, key: &str, body: &Path) -> Result<()> {
        let body = body
            .to_str()
            .with_context(|| format!("seal spool path is not valid UTF-8: {}", body.display()))?;
        self.aws(&[
            "s3api",
            "put-object",
            "--bucket",
            &self.bucket,
            "--key",
            key,
            "--body",
            body,
            "--output",
            "json",
        ])
        .await
        .map(|_| ())
    }

    async fn aws(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("aws")
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .context(
                "run the `aws` CLI — it is how the broker reaches the seal store, \
                 so it must be on PATH",
            )?;
        if !output.status.success() {
            // The full argv would echo the key, which is harmless, but the
            // subcommand alone is enough to place the failure and keeps a 9.5k
            // key listing out of the log line.
            bail!(
                "aws {} failed on s3://{}: {}",
                args.first().copied().unwrap_or("?"),
                self.bucket,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout).context("aws CLI emitted non-UTF-8 output")
    }
}

/// Parse `--output text` rows of `<key>\t<size>`.
///
/// An empty result set prints `None`, not an empty document, so that word is a
/// value to recognise rather than a key to choke on.
fn parse_listing(stdout: &str) -> Vec<Object> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "None")
        .filter_map(|line| {
            let (key, size) = line.rsplit_once('\t')?;
            Some(Object {
                key: key.trim().to_string(),
                size: size.trim().parse().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_rows_become_key_and_size() {
        let rows = "sessions/2026/08/03/abc/turns.jsonl.zst\t637\n\
                    sessions/2026/08/03/def/turns.jsonl.zst\t2881243686\n";
        assert_eq!(
            parse_listing(rows),
            vec![
                Object {
                    key: "sessions/2026/08/03/abc/turns.jsonl.zst".into(),
                    size: 637
                },
                Object {
                    key: "sessions/2026/08/03/def/turns.jsonl.zst".into(),
                    size: 2_881_243_686
                },
            ]
        );
    }

    // An empty bucket prints the literal word `None`, which must read as "no
    // objects" and not as a key — otherwise a first run against a fresh store
    // reports one nonsense seal.
    #[test]
    fn an_empty_listing_is_empty() {
        assert!(parse_listing("None\n").is_empty());
        assert!(parse_listing("").is_empty());
        assert!(parse_listing("\n\n").is_empty());
    }

    // Sizes beyond u32 are routine here: the largest seal is 2.88 GB.
    #[test]
    fn a_row_without_a_parseable_size_is_dropped_not_guessed() {
        assert!(parse_listing("sessions/a/turns.jsonl.zst\tnot-a-number").is_empty());
    }
}
