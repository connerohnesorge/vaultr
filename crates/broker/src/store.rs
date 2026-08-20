//! The seal store, reached through the `aws` CLI.
//!
//! Transport matches `vaultr::seals` — the read side of the same bucket — and
//! for the same reason it gives there: the hard part of reaching this store is
//! credential resolution, not the request. The broker resolves IRSA in athens,
//! SSO on the Mac while it is being proven locally, and environment variables in
//! CI, and the CLI already resolves all three. Each CLI process has a material
//! memory and CPU cost, so the store bounds their concurrency. A bulk reconcile
//! can otherwise fork enough existence checks to OOM the broker.
//!
//! The broker uses a small set of AWS CLI calls. Listing and upload preserve the
//! existing narrow write grant. Reads use `s3 presign` so the broker can return
//! a short-lived URL without buffering seal bytes or issuing `get-object` itself.
//!
//! - `list-objects-v2` returns key *and* size, which is everything an idempotent
//!   size-checked comparison needs. `head-object` would be the obvious call and
//!   is unnecessary for the write path.
//! - `put-object` is single-part. `aws s3 cp` switches to multipart above 8 MB,
//!   which drags in `s3:AbortMultipartUpload` to clean up after a failure;
//!   single-part keeps the write policy narrow, at the cost of a 5 GiB
//!   per-object ceiling that the largest seal on record (2.7 GB) sits under.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};

/// The store of record. Keys are the vault-relative path of the seal:
/// `sessions/YYYY/MM/DD/<session-id>/turns.jsonl.zst`, so a local path and its
/// S3 key are the same string and no mapping exists to get wrong.
pub const DEFAULT_BUCKET: &str = "pantheon-vault-seals-athens";

/// Every key the broker will ever touch sits under this prefix. Listing is
/// scoped to it so a bucket that later holds anything else stays invisible here.
pub const KEY_PREFIX: &str = "sessions/";

/// S3's ceiling for a single-part `PutObject`.
pub const MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Six AWS CLI children fit within the broker's production memory budget while
/// leaving enough parallelism for a large first reconcile to make progress.
pub const DEFAULT_MAX_AWS_PROCESSES: usize = 6;

#[derive(Debug)]
pub struct Store {
    bucket: String,
    aws_binary: PathBuf,
    aws_processes: Semaphore,
}

/// One object as the store reports it: the two fields idempotence needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
}

impl Store {
    pub fn with_max_aws_processes(bucket: impl Into<String>, max_aws_processes: usize) -> Self {
        Self::with_aws_binary(bucket, max_aws_processes, "aws")
    }

    pub(crate) fn with_aws_binary(
        bucket: impl Into<String>,
        max_aws_processes: usize,
        aws_binary: impl Into<PathBuf>,
    ) -> Self {
        assert!(
            max_aws_processes > 0,
            "the AWS process limit must be positive"
        );
        Store {
            bucket: bucket.into(),
            aws_binary: aws_binary.into(),
            aws_processes: Semaphore::new(max_aws_processes),
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

    /// Return a short-lived URL that authorizes one object read.
    pub async fn presign(&self, key: &str, expires_in: u64) -> Result<String> {
        let object = format!("s3://{}/{}", self.bucket, key);
        let expires_in = expires_in.to_string();
        let output = self
            .aws(&["s3", "presign", &object, "--expires-in", &expires_in])
            .await?;
        let url = output.trim();
        if url.is_empty() {
            bail!("aws s3 presign returned an empty URL for {object}");
        }
        Ok(url.to_string())
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
        // The request body is already on disk before an upload reaches here, so
        // waiting applies backpressure without retaining seal bytes in memory.
        // Every AWS CLI child passes through this one permit boundary, including
        // listings, presign checks, and writes.
        let _permit = self.aws_process_permit().await;
        let output = Command::new(&self.aws_binary)
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

    async fn aws_process_permit(&self) -> SemaphorePermit<'_> {
        self.aws_processes
            .acquire()
            .await
            .expect("the store never closes its AWS process semaphore")
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

    #[tokio::test]
    async fn aws_processes_are_bounded() {
        let store = Store::with_max_aws_processes("test", 2);
        let first = store.aws_process_permit().await;
        let second = store.aws_process_permit().await;

        assert!(store.aws_processes.try_acquire().is_err());
        drop(first);
        assert!(store.aws_processes.try_acquire().is_ok());
        drop(second);
    }

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
