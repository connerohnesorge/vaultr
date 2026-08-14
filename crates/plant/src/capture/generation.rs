//! Capture-owned immutable-generation maintenance. This module alone crosses
//! strict persistence readiness into Capture/Herdr detachment and Sealing.

use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use super::persistence::{canonical_root, sealing_readiness, session_lock, SealReadiness};
use super::session_fs::{clone_at_start, hash_file, SessionDirectory};

pub(crate) struct SealedCapture {
    pub(crate) path: PathBuf,
    pub(crate) source_len: u64,
}

fn secrets_policy(directory: &SessionDirectory) -> Result<vaultr::secrets::Policy, String> {
    let root = directory
        .path()
        .ancestors()
        .find(|path| path.join(".secretsignore").is_file())
        .unwrap_or_else(|| directory.path());
    vaultr::secrets::policy_for(root)
        .map_err(|error| format!("load secret policy from {}: {error:#}", root.display()))
}

fn scrub_entry(directory: &SessionDirectory, name: &str) -> Result<(File, usize), String> {
    let legacy_temps: &[&str] = if name == "turns.jsonl" {
        &["turns.scrub-tmp"]
    } else {
        &[]
    };
    directory.cleanup_temps(name, &["scrub"], legacy_temps, &[])?;
    let source = directory.open_required(name, true)?;
    let mut needles = HashSet::new();
    let denylist = format!(
        "{}/.config/wireproxy/scrub-denylist.txt",
        std::env::var("HOME").unwrap_or_default()
    );
    if let Ok(contents) = std::fs::read_to_string(&denylist) {
        for line in contents.lines() {
            let value = line.trim();
            if value.len() >= 6 {
                let escaped = serde_json::to_string(value).unwrap_or_default();
                needles.insert(value.to_string());
                if escaped.len() >= 2 {
                    needles.insert(escaped[1..escaped.len() - 1].to_string());
                }
            }
        }
    }
    let policy = secrets_policy(directory)?;
    let (temporary_name, temporary) = directory.create_temp(name, "scrub")?;
    let scrubbed = (|| -> Result<usize, String> {
        let reader = BufReader::new(clone_at_start(&source)?);
        let mut writer = BufWriter::new(
            temporary
                .try_clone()
                .map_err(|error| format!("clone scrub temp: {error}"))?,
        );
        let mut hits = 0;
        // One JSON-aware pass for both the denylist needles and the pattern
        // matcher. Byte-replacing either one inside a serialized envelope is
        // what ate the backslash of an escaped quote and left 15 seals with an
        // unparseable record.
        let needles: Vec<String> = needles.iter().cloned().collect();
        for line in reader.lines() {
            let line = line.map_err(|error| format!("read session capture: {error}"))?;
            let (line, count) = vaultr::secrets::redact_capture_line(&line, &needles, &policy);
            hits += count;
            writeln!(writer, "{line}")
                .map_err(|error| format!("write scrubbed session capture: {error}"))?;
        }
        writer
            .flush()
            .map_err(|error| format!("flush scrubbed session capture: {error}"))?;
        temporary
            .sync_all()
            .map_err(|error| format!("sync scrubbed session capture: {error}"))?;
        Ok(hits)
    })();
    let hits = match scrubbed {
        Ok(hits) => hits,
        Err(error) => {
            let _ = directory.unlink_if_same(&temporary_name, &temporary);
            return Err(error);
        }
    };
    if hits == 0 {
        directory.unlink_if_same(&temporary_name, &temporary)?;
        return Ok((source, 0));
    }
    match directory.replace_entry(&temporary_name, name, &temporary, Some(&source)) {
        Ok(scrubbed) => Ok((scrubbed, hits)),
        Err(error) => {
            let _ = directory.unlink_if_same(&temporary_name, &temporary);
            Err(error)
        }
    }
}

fn print_scrub(path: &Path, hits: usize) {
    if hits == 0 {
        return;
    }
    let path = path.display().to_string();
    let relative = path.split("/sessions/").nth(1).unwrap_or(&path);
    println!("[scrub] {relative}: {hits} redaction(s)");
}

pub(crate) async fn scrub(path: &Path) -> bool {
    let Some(directory_path) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Ok(directory) = SessionDirectory::open(directory_path) else {
        return false;
    };
    if directory.lock_exclusive().is_err() {
        return false;
    }
    let Ok((_, hits)) = scrub_entry(&directory, name) else {
        return false;
    };
    print_scrub(path, hits);
    true
}

fn detached_sidecar(
    directory: &SessionDirectory,
) -> Result<Option<vaultr::vault::DetachedGeneration>, String> {
    let directory_path = directory.path();
    const RAW: &str = "herdr.jsonl";
    const DEST: &str = "herdr.jsonl.zst";
    let prefix = format!("{RAW}.sealing-");
    let mut detached = None;
    for name in directory.entry_names()? {
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let path = directory_path.join(&name);
        let (base_len, digest) = suffix
            .split_once('-')
            .ok_or_else(|| format!("invalid detached sidecar at {}", path.display()))?;
        let base_len = base_len
            .parse::<u64>()
            .map_err(|_| format!("invalid detached sidecar at {}", path.display()))?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("invalid detached sidecar at {}", path.display()));
        }
        let digest = digest.to_ascii_lowercase();
        let file = directory.open_required(&name, false)?;
        if hash_file(&file)? != digest {
            return Err(format!(
                "detached sidecar digest mismatch at {}",
                path.display()
            ));
        }
        if detached
            .replace(vaultr::vault::DetachedGeneration {
                path,
                base_len,
                digest,
            })
            .is_some()
        {
            return Err(format!(
                "multiple detached sidecar generations under {}",
                directory_path.display()
            ));
        }
    }
    if detached.is_some() {
        return Ok(detached);
    }

    let Some(source) = directory.open_optional(RAW, true)? else {
        return Ok(None);
    };
    let base_len = directory
        .open_optional(DEST, false)?
        .map(|file| file.metadata())
        .transpose()
        .map_err(|error| {
            format!(
                "inspect sealed sidecar {}: {error}",
                directory_path.join(DEST).display()
            )
        })?
        .map_or(0, |metadata| metadata.len());
    let digest = hash_file(&source)?;
    let detached_name = format!("{RAW}.sealing-{base_len}-{digest}");
    directory.replace_entry(RAW, &detached_name, &source, None)?;
    Ok(Some(vaultr::vault::DetachedGeneration {
        path: directory_path.join(detached_name),
        base_len,
        digest,
    }))
}

fn detach_capture(
    directory: &SessionDirectory,
) -> Result<vaultr::vault::DetachedGeneration, String> {
    let directory_path = directory.path();
    let (source, hits) = scrub_entry(directory, "turns.jsonl")?;
    print_scrub(&directory_path.join("turns.jsonl"), hits);
    let base_len = directory
        .open_optional("turns.jsonl.zst", false)?
        .map(|file| file.metadata())
        .transpose()
        .map_err(|error| {
            format!(
                "inspect sealed generation under {}: {error}",
                directory_path.display()
            )
        })?
        .map_or(0, |metadata| metadata.len());
    let digest = hash_file(&source)?;
    let detached_name = format!("turns.jsonl.sealing-{base_len}-{digest}");
    directory.replace_entry("turns.jsonl", &detached_name, &source, None)?;
    Ok(vaultr::vault::DetachedGeneration {
        path: directory_path.join(detached_name),
        base_len,
        digest,
    })
}

async fn run_frame_command(
    mut command: tokio::process::Command,
    source: &File,
    frame: &File,
    timeout: Duration,
    inherit_stderr: bool,
) -> Result<(), String> {
    frame
        .set_len(0)
        .map_err(|error| format!("truncate compression temp: {error}"))?;
    let input = clone_at_start(source)?;
    let output = clone_at_start(frame)?;
    command
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output));
    command.stderr(if inherit_stderr {
        Stdio::inherit()
    } else {
        Stdio::piped()
    });
    let result = crate::process::run_command(&mut command, timeout).await;
    if !result.ok {
        return Err(format!("zstd {}", result.failure_detail()));
    }
    frame
        .sync_all()
        .map_err(|error| format!("sync compression temp: {error}"))
}

async fn compress_frame_with_timeout(
    source: &File,
    frame: &File,
    timeout: Duration,
) -> Result<(), String> {
    let source_len = source
        .metadata()
        .map_err(|error| format!("inspect detached generation: {error}"))?
        .len();
    let stream_size = format!("--stream-size={source_len}");
    let mut command = tokio::process::Command::new("zstd");
    command
        .args(["-19", "-T0", "-q", "-c", &stream_size])
        .env("PATH", crate::process::augmented_path());
    run_frame_command(command, source, frame, timeout, true).await
}

#[derive(Clone, Copy)]
enum FrameCompressor {
    Zstd,
    #[cfg(test)]
    CorruptSuccess,
    #[cfg(test)]
    InheritedStderr,
}

async fn write_frame(
    compressor: FrameCompressor,
    source: &File,
    frame: &File,
) -> Result<(), String> {
    match compressor {
        FrameCompressor::Zstd => {
            compress_frame_with_timeout(source, frame, Duration::from_secs(600)).await
        }
        #[cfg(test)]
        FrameCompressor::CorruptSuccess => {
            let mut output = clone_at_start(frame)?;
            output
                .write_all(b"not a zstd frame")
                .and_then(|_| output.flush())
                .map_err(|error| format!("write corrupt-success fixture: {error}"))?;
            frame
                .sync_all()
                .map_err(|error| format!("sync corrupt-success fixture: {error}"))
        }
        #[cfg(test)]
        FrameCompressor::InheritedStderr => {
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", "sleep 5 >&2 & exit 0"]);
            run_frame_command(command, source, frame, Duration::from_millis(50), false).await
        }
    }
}

#[cfg(test)]
async fn seal_generation(
    generation: &vaultr::vault::DetachedGeneration,
    destination: &Path,
) -> Result<PathBuf, String> {
    seal_generation_with(generation, destination, FrameCompressor::Zstd).await
}

#[cfg(test)]
async fn seal_generation_with(
    generation: &vaultr::vault::DetachedGeneration,
    destination: &Path,
    compressor: FrameCompressor,
) -> Result<PathBuf, String> {
    let directory_path = generation.path.parent().ok_or_else(|| {
        format!(
            "detached generation has no directory at {}",
            generation.path.display()
        )
    })?;
    let directory = SessionDirectory::open(directory_path)?;
    directory.lock_exclusive()?;
    let result = seal_generation_in(&directory, generation, destination, compressor).await;
    drop(directory);
    result
}

async fn seal_generation_in(
    directory: &SessionDirectory,
    generation: &vaultr::vault::DetachedGeneration,
    destination: &Path,
    compressor: FrameCompressor,
) -> Result<PathBuf, String> {
    let directory_path = generation.path.parent().ok_or_else(|| {
        format!(
            "detached generation has no directory at {}",
            generation.path.display()
        )
    })?;
    if directory.path() != directory_path {
        return Err(format!(
            "detached generation leaves retained session directory at {}",
            generation.path.display()
        ));
    }
    if destination.parent() != Some(directory_path) {
        return Err(format!(
            "sealed destination leaves session directory at {}",
            destination.display()
        ));
    }
    let source_name = generation
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "invalid detached generation name at {}",
                generation.path.display()
            )
        })?;
    let destination_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "invalid sealed generation name at {}",
                destination.display()
            )
        })?;
    let source = directory.open_required(source_name, true)?;
    source.sync_all().map_err(|error| {
        format!(
            "sync detached generation {}: {error}",
            generation.path.display()
        )
    })?;
    if hash_file(&source)? != generation.digest {
        return Err(format!(
            "detached generation digest mismatch at {}",
            generation.path.display()
        ));
    }
    let raw_mtime = source
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|error| {
            format!(
                "inspect detached generation {}: {error}",
                generation.path.display()
            )
        })?;

    let (legacy_temps, forbidden_temps): (&[&str], &[&str]) = match destination_name {
        "turns.jsonl.zst" => (&["turns.jsonl.frame-tmp", "turns.jsonl.zst-tmp"], &[]),
        // Path::with_extension on the parent implementation's herdr.jsonl
        // source emitted these exact names.
        "herdr.jsonl.zst" => (
            &["herdr.frame-tmp", "herdr.zst-tmp"],
            &["herdr.jsonl.frame-tmp", "herdr.jsonl.zst-tmp"],
        ),
        _ => (&[], &[]),
    };
    directory.cleanup_temps(
        destination_name,
        &["frame", "merged"],
        legacy_temps,
        forbidden_temps,
    )?;
    let prior = directory.open_optional(destination_name, false)?;
    let prior_len = prior
        .as_ref()
        .map(|file| file.metadata())
        .transpose()
        .map_err(|error| {
            format!(
                "inspect sealed destination {}: {error}",
                destination.display()
            )
        })?
        .map_or(0, |metadata| metadata.len());
    let committed = if prior_len > generation.base_len {
        prior.expect("positive destination length")
    } else if prior_len == generation.base_len {
        let (frame_name, frame) = directory.create_temp(destination_name, "frame")?;
        if let Err(error) = write_frame(compressor, &source, &frame).await {
            let _ = directory.unlink_if_same(&frame_name, &frame);
            return Err(error);
        }
        let (merged_name, merged) = match directory.create_temp(destination_name, "merged") {
            Ok(merged) => merged,
            Err(error) => {
                let _ = directory.unlink_if_same(&frame_name, &frame);
                return Err(error);
            }
        };
        let assembled = (|| -> Result<(), String> {
            let mut output = clone_at_start(&merged)?;
            if let Some(prior) = &prior {
                std::io::copy(&mut clone_at_start(prior)?, &mut output)
                    .map_err(|error| format!("copy sealed generation: {error}"))?;
            }
            std::io::copy(&mut clone_at_start(&frame)?, &mut output)
                .map_err(|error| format!("copy compressed generation: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("flush merged generation: {error}"))?;
            merged
                .sync_all()
                .map_err(|error| format!("sync merged generation: {error}"))
        })();
        if let Err(error) = assembled {
            let _ = directory.unlink_if_same(&merged_name, &merged);
            let _ = directory.unlink_if_same(&frame_name, &frame);
            return Err(error);
        }
        let renamed =
            directory.replace_entry(&merged_name, destination_name, &merged, prior.as_ref());
        if renamed.is_err() {
            let _ = directory.unlink_if_same(&merged_name, &merged);
        }
        let frame_cleanup = directory.unlink_if_same(&frame_name, &frame);
        let committed = renamed?;
        frame_cleanup?;
        committed
    } else {
        return Err(format!(
            "sealed destination conflicts with detached generation at {}",
            destination.display()
        ));
    };
    let decoded_digest =
        vaultr::vault::decoded_zstd_suffix_digest(clone_at_start(&committed)?, generation.base_len)
            .map_err(|_| {
                format!(
                    "sealed destination suffix is invalid at {}",
                    destination.display()
                )
            })?;
    if decoded_digest != generation.digest {
        return Err(format!(
            "sealed destination conflicts with detached generation at {}",
            destination.display()
        ));
    }
    committed
        .set_modified(raw_mtime)
        .map_err(|error| format!("set mtime {}: {error}", destination.display()))?;
    committed
        .sync_all()
        .map_err(|error| format!("sync sealed destination {}: {error}", destination.display()))?;
    if !directory.entry_matches(destination_name, &committed)? {
        return Err(format!(
            "sealed destination changed before detached cleanup at {}",
            destination.display()
        ));
    }
    directory.sync()?;
    directory.unlink_if_same(source_name, &source)?;
    Ok(destination.to_path_buf())
}

/// Capture-owned claim/readiness, detachment, sidecar, and exact-once Sealing
/// transaction. Sweep supplies policy only.
pub(crate) async fn seal_ready_generation(
    vault: &Path,
    sid: &str,
    directory_path: &Path,
) -> Result<Option<SealedCapture>, String> {
    let root = canonical_root(vault);
    let lock = session_lock(&root, sid);
    let (directory, capture, herdr, source_len) = {
        let _guard = lock.lock().await;
        let directory = SessionDirectory::open(directory_path)?;
        directory.lock_exclusive()?;
        let Some(readiness) = sealing_readiness(&root, sid, directory_path)? else {
            return Ok(None);
        };
        let capture = match readiness {
            SealReadiness::Detached(generation) => generation,
            SealReadiness::Raw => detach_capture(&directory)?,
        };
        let herdr = detached_sidecar(&directory)?;
        let source_name = capture
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                format!(
                    "invalid detached generation name at {}",
                    capture.path.display()
                )
            })?;
        let source_len = directory
            .open_required(source_name, false)?
            .metadata()
            .map_err(|error| {
                format!(
                    "inspect detached generation {}: {error}",
                    capture.path.display()
                )
            })?
            .len();
        (directory, capture, herdr, source_len)
    };

    // Scrub the complete request-body journal before the detached capture is
    // committed. If scrubbing fails, the detached generation remains eligible
    // for a safe retry instead of leaving a sealed session with raw state.
    if directory.open_optional("state.json", false)?.is_some() {
        let (_, hits) = scrub_entry(&directory, "state.json")?;
        print_scrub(&directory_path.join("state.json"), hits);
    }
    if let Some(herdr) = herdr {
        let destination = directory_path.join("herdr.jsonl.zst");
        seal_generation_in(&directory, &herdr, &destination, FrameCompressor::Zstd)
            .await
            .map_err(|error| format!("seal {sid} herdr.jsonl: {error}"))?;
    }
    let destination = directory_path.join("turns.jsonl.zst");
    let path = seal_generation_in(&directory, &capture, &destination, FrameCompressor::Zstd)
        .await
        .map_err(|error| format!("seal detached generation for {sid}: {error}"))?;
    Ok(Some(SealedCapture { path, source_len }))
}

fn last_herdr_snapshot(file: File) -> String {
    let Some(line) = BufReader::new(file).lines().last().and_then(Result::ok) else {
        return String::new();
    };
    let Ok(Value::Object(mut snapshot)) = serde_json::from_str::<Value>(&line) else {
        return String::new();
    };
    snapshot.remove("ts");
    serde_json::to_string(&Value::Object(snapshot)).unwrap_or_default()
}

pub(crate) fn current_herdr_snapshot(vault: &Path, sid: &str) -> String {
    super::session_dir(vault, sid)
        .ok()
        .and_then(|path| SessionDirectory::open(&path).ok())
        .and_then(|directory| directory.open_optional("herdr.jsonl", false).ok().flatten())
        .map(last_herdr_snapshot)
        .unwrap_or_default()
}

pub(crate) async fn append_herdr_snapshot(
    vault: &Path,
    sid: &str,
    snapshot_without_timestamp: &str,
    line: &str,
) -> Result<bool, String> {
    let root = canonical_root(vault);
    let lock = session_lock(&root, sid);
    let _guard = lock.lock().await;
    let directory_path = super::session_dir(vault, sid)
        .map_err(|error| format!("resolve Herdr session: {error}"))?;
    let path = directory_path.join("herdr.jsonl");
    let directory = SessionDirectory::open(&directory_path)?;
    if directory
        .open_optional("herdr.jsonl", false)?
        .map(last_herdr_snapshot)
        .unwrap_or_default()
        == snapshot_without_timestamp
    {
        return Ok(false);
    }
    let mut file = directory
        .open_append("herdr.jsonl", true)?
        .expect("create=true returns a sidecar handle");
    writeln!(file, "{line}")
        .map_err(|error| format!("append Herdr snapshot at {}: {error}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests;
