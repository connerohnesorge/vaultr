//! Fetch-on-miss for sealed captures that live in S3 rather than on local disk.
//!
//! Seals left git history for an S3 store of record, so a host can hold
//! `sessions/.meta/<id>.json` — which is still in git, and is what keeps every
//! session listed and discoverable by clone — without holding that session's
//! bytes. This module turns "listed" back into "readable".
//!
//! Only the read verbs (`session show`, `session path`, `session fork`, and
//! `session herdr`) fetch. Nothing on the capture or sweep path does,
//! deliberately: Plant walks all
//! ~10k session directories on a 30-minute cadence (`compress`, `learn`,
//! `reconcile`, `validate`), and a fetch reachable from that walk would turn an
//! eligibility scan into a bulk download of the whole corpus.
//!
//! Transport is the `aws` CLI rather than an SDK. The hard part of reaching this
//! bucket is credential resolution, not the GET — the Mac authenticates through
//! SSO cache, a pod through IRSA, CI through environment variables — and the CLI
//! already resolves all three. An SDK would also drag tokio and hyper into what
//! is otherwise a fully synchronous, dependency-light binary.

use crate::vault::{self, CaptureGenerations, Session};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The store of record. Every key is the seal's vault-relative path.
const DEFAULT_STORE: &str = "s3://pantheon-vault-seals-athens";

/// Zstd frame magic. A seal that does not start with it is not a seal — the
/// cheap half of verifying a download that is far too large to decode.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SealClass {
    Capture,
    Herdr,
}

impl SealClass {
    fn filename(self) -> &'static str {
        match self {
            Self::Capture => "turns.jsonl.zst",
            Self::Herdr => "herdr.jsonl.zst",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Herdr => "Herdr sidecar",
        }
    }
}

/// A resolved seal store: just the bucket, since the key layout is fixed.
#[derive(Debug, Clone)]
pub struct SealStore {
    bucket: String,
}

/// Resolve the seal store from `$VAULTR_SEAL_STORE` (an `s3://bucket` URI),
/// defaulting to the athens store. Setting it empty disables fetching.
pub fn store() -> Result<Option<SealStore>> {
    let raw = match std::env::var("VAULTR_SEAL_STORE") {
        Ok(value) => value,
        Err(_) => DEFAULT_STORE.to_string(),
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let bucket = raw
        .strip_prefix("s3://")
        .with_context(|| format!("VAULTR_SEAL_STORE must be an s3:// URI, got {raw:?}"))?
        .trim_end_matches('/');
    if bucket.is_empty() || bucket.contains('/') {
        bail!("VAULTR_SEAL_STORE must name a bucket with no key prefix, got {raw:?}");
    }
    Ok(Some(SealStore {
        bucket: bucket.to_string(),
    }))
}

/// One place a seal might be, and where it lands locally if it is there.
#[derive(Debug)]
struct Candidate {
    key: String,
    dir: PathBuf,
}

/// The keys a session's seal could hold, best guess first.
///
/// A seal's key is the path its file had on disk when it was migrated, and that
/// path is not always the one `original_start` derives: of the 9,435 seals
/// measured on 2026-08-11, 31 sit exactly one day earlier than the UTC date in
/// `.meta`, because their directory was placed by *local* date while `.meta`
/// records UTC (every one of them 2026-07-13 on disk against 2026-07-14 in meta,
/// with starts between 00:22Z and 01:47Z). A timezone offset is under 24 hours
/// by construction, so probing the neighbouring days closes that class exactly
/// rather than approximately.
fn candidates(root: &Path, session: &Session, seal: SealClass) -> Result<Vec<Candidate>> {
    let start = session.meta.original_start.as_deref().with_context(|| {
        format!(
            "session {} records no original_start, so its seal key cannot be derived",
            session.id
        )
    })?;
    let date = chrono::DateTime::parse_from_rfc3339(start)
        .with_context(|| format!("session {} has an unparseable original_start", session.id))?
        .with_timezone(&chrono::Utc)
        .date_naive();
    Ok([0i64, -1, 1]
        .into_iter()
        .filter_map(|offset| date.checked_add_signed(chrono::Duration::days(offset)))
        .map(|date| {
            let (year, month, day) = (
                date.format("%Y").to_string(),
                date.format("%m").to_string(),
                date.format("%d").to_string(),
            );
            Candidate {
                key: format!(
                    "sessions/{year}/{month}/{day}/{}/{}",
                    session.id,
                    seal.filename()
                ),
                dir: root.join(year).join(month).join(day).join(&session.id),
            }
        })
        .collect())
}

/// A session's capture file, and the directory holding it.
pub struct Materialised {
    pub dir: PathBuf,
    pub capture: PathBuf,
}

/// Resolve a session's capture, fetching its seal from the store when the local
/// vault does not hold it.
///
/// A local hit always wins and never touches the network. Only genuine absence
/// falls through to a fetch: a structural hazard (a symlinked path level, a
/// capture generation that is not a regular file, a detached digest mismatch)
/// still fails loudly, because absence is recoverable and a hazard is not.
pub fn materialise(root: &Path, session: &Session, allow_fetch: bool) -> Result<Materialised> {
    if let Some(dir) = vault::find_session_dir(root, session)? {
        if let Some(capture) = CaptureGenerations::load(&dir)?.capture_file() {
            return Ok(Materialised {
                dir: dir.clone(),
                capture: capture.to_path_buf(),
            });
        }
    }
    if !allow_fetch {
        bail!(
            "session {} has no capture in {} and fetching is disabled",
            session.id,
            root.display()
        );
    }
    let capture = fetch_seal(root, session, SealClass::Capture)?;
    let dir = capture
        .parent()
        .with_context(|| {
            format!(
                "fetched seal has no parent directory: {}",
                capture.display()
            )
        })?
        .to_path_buf();
    Ok(Materialised { dir, capture })
}

/// A local Herdr topology sidecar and whether it needs zstd decoding.
pub struct HerdrMaterialised {
    pub sidecar: PathBuf,
    pub compressed: bool,
}

/// Resolve a session's Herdr topology, fetching only that sidecar on a miss.
///
/// Raw topology wins while Plant still owns an appendable sidecar. Explicit
/// inspection is the only caller; recurring capture and cultivation inventory
/// remains local-only.
pub fn materialise_herdr(
    root: &Path,
    session: &Session,
    allow_fetch: bool,
) -> Result<HerdrMaterialised> {
    if let Some(dir) = vault::find_session_dir(root, session)? {
        for (filename, compressed) in [("herdr.jsonl", false), ("herdr.jsonl.zst", true)] {
            let path = dir.join(filename);
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    std::fs::File::open(&path)
                        .with_context(|| format!("open Herdr sidecar at {}", path.display()))?;
                    return Ok(HerdrMaterialised {
                        sidecar: path,
                        compressed,
                    });
                }
                Ok(_) => bail!("Herdr sidecar is not a regular file at {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect Herdr sidecar at {}", path.display()));
                }
            }
        }
    }
    if !allow_fetch {
        bail!(
            "session {} has no Herdr sidecar in {} and fetching is disabled",
            session.id,
            root.display()
        );
    }
    Ok(HerdrMaterialised {
        sidecar: fetch_seal(root, session, SealClass::Herdr)?,
        compressed: true,
    })
}

/// Download one seal class from the store and return its local path.
///
/// The seal is written under a name that is invisible to everything else, then
/// verified, then renamed into place. Capture parsing skips the temp name, and
/// the `.zst-tmp` suffix is already gitignored, so a crashed fetch cannot be
/// swept into a commit.
fn fetch_seal(root: &Path, session: &Session, seal: SealClass) -> Result<PathBuf> {
    let Some(store) = store()? else {
        bail!(
            "session {} has no local {} and the seal store is disabled \
             (VAULTR_SEAL_STORE is empty)",
            session.id,
            seal.label()
        );
    };
    let candidates = candidates(root, session, seal)?;
    let mut tried = Vec::new();
    for candidate in &candidates {
        tried.push(candidate.key.clone());
        let Some(size) = head_object(&store, &candidate.key)? else {
            continue;
        };
        return download(&store, candidate, seal, size, &session.id);
    }
    bail!(
        "{} for session {} is in neither the local vault nor s3://{}\n  tried: {}",
        seal.label(),
        session.id,
        store.bucket,
        tried.join("\n         ")
    )
}

/// The object's size, or `None` when it is not there.
///
/// A 404 is an answer; anything else — expired SSO, a denied bucket, no `aws` on
/// PATH — is a failure to report rather than a key to skip past.
fn head_object(store: &SealStore, key: &str) -> Result<Option<u64>> {
    let output = Command::new("aws")
        .args([
            "s3api",
            "head-object",
            "--bucket",
            &store.bucket,
            "--key",
            key,
            "--output",
            "json",
        ])
        .output()
        .context(
            "run `aws s3api head-object` — the aws CLI is how vaultr reaches the seal store, \
             so it must be on PATH",
        )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("(404)") || stderr.contains("Not Found") {
            return Ok(None);
        }
        bail!("head s3://{}/{key} failed: {}", store.bucket, stderr.trim());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse head-object response")?;
    value
        .get("ContentLength")
        .and_then(serde_json::Value::as_u64)
        .map(Some)
        .context("head-object response carries no ContentLength")
}

fn download(
    store: &SealStore,
    candidate: &Candidate,
    seal: SealClass,
    expected: u64,
    session_id: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(&candidate.dir)
        .with_context(|| format!("create session directory {}", candidate.dir.display()))?;
    let dest = candidate.dir.join(seal.filename());
    // Staged in the destination directory so the rename is same-filesystem and
    // therefore atomic; a temp directory could be on another volume.
    let stem = seal
        .filename()
        .strip_suffix(".zst")
        .expect("every seal filename is zstd");
    let tmp = candidate.dir.join(format!(
        "{stem}.fetch-{}.zst-tmp",
        uuid::Uuid::new_v4().simple()
    ));
    // Announced before the transfer rather than reported after it, and with the
    // size, because seals run from a few hundred bytes to 2.88 GB and a silent
    // multi-minute download reads as a hang.
    eprintln!(
        "fetching {} seal for {session_id} from s3://{}/{}",
        human_bytes(expected),
        store.bucket,
        candidate.key
    );
    let result = (|| -> Result<()> {
        // `aws s3 cp` writes its progress and its completion line to STDOUT, so
        // both are suppressed and stdout is nulled outright: `session path`
        // prints a path that gets piped into `cd` and copied to the clipboard,
        // and one line of transfer chatter on stdout corrupts it.
        let status = Command::new("aws")
            .arg("s3")
            .arg("cp")
            .arg("--quiet")
            .arg(format!("s3://{}/{}", store.bucket, candidate.key))
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .status()
            .context("run `aws s3 cp`")?;
        if !status.success() {
            bail!(
                "aws s3 cp s3://{}/{} exited with {status}",
                store.bucket,
                candidate.key
            );
        }
        verify(&tmp, expected)?;
        // Never clobber local evidence that arrived while the object downloaded.
        let raw_herdr = candidate.dir.join("herdr.jsonl");
        if dest.exists() || (seal == SealClass::Herdr && raw_herdr.exists()) {
            bail!("{} appeared during fetch", seal.label());
        }
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename fetched seal into {}", dest.display()))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(dest)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Confirm the download is the whole object and is a zstd stream.
///
/// Size is the check that matters: a truncated transfer is the realistic
/// failure, and `aws s3 cp` already validates the object's own checksum on the
/// way down. The seal is deliberately not decoded — these compress ~47x, so
/// verifying the largest one by decompression would mean pushing ~135 GB
/// through zstd to read a single session.
fn verify(path: &Path, expected: u64) -> Result<()> {
    let actual = std::fs::metadata(path)
        .with_context(|| format!("stat fetched seal {}", path.display()))?
        .len();
    if actual != expected {
        bail!("fetched seal is {actual} bytes, expected {expected} — refusing a truncated capture");
    }
    let mut head = [0u8; 4];
    {
        use std::io::Read;
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("open fetched seal {}", path.display()))?;
        file.read_exact(&mut head)
            .with_context(|| format!("read zstd header from {}", path.display()))?;
    }
    if head != ZSTD_MAGIC {
        bail!("fetched seal is not a zstd stream (magic {head:02x?})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Meta;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn session(id: &str, start: Option<&str>) -> Session {
        Session {
            id: id.to_string(),
            meta: Meta {
                original_start: start.map(String::from),
                ..Default::default()
            },
        }
    }

    #[test]
    fn candidate_keys_lead_with_the_meta_derived_date() {
        let session = session("abc", Some("2026-07-14T01:47:38.711Z"));
        let candidates =
            candidates(Path::new("/vault/sessions"), &session, SealClass::Capture).unwrap();
        let keys: Vec<&str> = candidates.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "sessions/2026/07/14/abc/turns.jsonl.zst",
                "sessions/2026/07/13/abc/turns.jsonl.zst",
                "sessions/2026/07/15/abc/turns.jsonl.zst",
            ]
        );
        assert_eq!(
            candidates[1].dir,
            Path::new("/vault/sessions/2026/07/13/abc")
        );
        assert_eq!(
            super::candidates(Path::new("/vault/sessions"), &session, SealClass::Herdr).unwrap()[0]
                .key,
            "sessions/2026/07/14/abc/herdr.jsonl.zst"
        );
    }

    #[test]
    fn candidate_keys_cross_month_and_year_boundaries() {
        let session = session("abc", Some("2026-01-01T00:10:00Z"));
        let keys: Vec<String> = candidates(Path::new("/v"), &session, SealClass::Capture)
            .unwrap()
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert!(keys.contains(&"sessions/2025/12/31/abc/turns.jsonl.zst".to_string()));
    }

    #[test]
    fn a_session_without_a_start_cannot_derive_a_key() {
        let error =
            candidates(Path::new("/v"), &session("abc", None), SealClass::Capture).unwrap_err();
        assert!(error.to_string().contains("no original_start"));
    }

    #[test]
    fn an_empty_store_setting_disables_fetching() {
        temp_env("VAULTR_SEAL_STORE", Some(""), || {
            assert!(store().unwrap().is_none());
        });
    }

    #[test]
    fn a_store_setting_must_be_a_bucket_uri() {
        temp_env(
            "VAULTR_SEAL_STORE",
            Some("pantheon-vault-seals-athens"),
            || {
                assert!(store().is_err());
            },
        );
        temp_env("VAULTR_SEAL_STORE", Some("s3://bucket/prefix"), || {
            assert!(store().is_err());
        });
        temp_env("VAULTR_SEAL_STORE", Some("s3://bucket"), || {
            assert_eq!(store().unwrap().unwrap().bucket, "bucket");
        });
    }

    #[test]
    fn human_bytes_spans_a_seal_corpus() {
        assert_eq!(human_bytes(637), "637 B");
        assert_eq!(human_bytes(485_600), "485.6 KB");
        assert_eq!(human_bytes(2_881_243_686), "2.9 GB");
    }

    #[test]
    fn verify_rejects_a_truncated_or_non_zstd_download() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seal");
        std::fs::write(&path, [0x28, 0xb5, 0x2f, 0xfd, 0x00]).unwrap();
        assert!(verify(&path, 5).is_ok());
        assert!(verify(&path, 6)
            .unwrap_err()
            .to_string()
            .contains("truncated"));
        std::fs::write(&path, b"<?xml error").unwrap();
        assert!(verify(&path, 11)
            .unwrap_err()
            .to_string()
            .contains("not a zstd stream"));
    }

    /// Environment is process-global; these tests set one variable and restore it.
    fn temp_env(key: &str, value: Option<&str>, body: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        body();
        match previous {
            Some(previous) => std::env::set_var(key, previous),
            None => std::env::remove_var(key),
        }
    }
}
