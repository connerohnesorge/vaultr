use super::*;
use crate::process::{run, which};
use std::io::Write;

fn detached(
    root: &Path,
    stem: &str,
    body: &[u8],
    base_len: u64,
) -> vaultr::vault::DetachedGeneration {
    let digest = vaultr::vault::sha256_hex(body);
    let path = root.join(format!("{stem}.sealing-{base_len}-{digest}"));
    std::fs::write(&path, body).unwrap();
    vaultr::vault::DetachedGeneration {
        path,
        base_len,
        digest,
    }
}

#[cfg(unix)]
#[test]
fn detachment_rejects_symlinked_capture_and_sidecar_sources() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("plant-detach-symlink-{}", uuid::Uuid::new_v4()));
    let session = root.join("session");
    std::fs::create_dir_all(&session).unwrap();
    let outside = root.join("outside");
    std::fs::write(&outside, b"outside evidence\n").unwrap();
    let outside_before = std::fs::read(&outside).unwrap();
    let directory = SessionDirectory::open(&session).unwrap();
    directory.lock_exclusive().unwrap();

    symlink(&outside, session.join("turns.jsonl")).unwrap();
    assert!(detach_capture(&directory).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), outside_before);

    std::fs::remove_file(session.join("turns.jsonl")).unwrap();
    symlink(&outside, session.join("herdr.jsonl")).unwrap();
    assert!(detached_sidecar(&directory).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn sealing_rejects_symlinked_source_and_destination_without_mutating_targets() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("plant-seal-symlink-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("turns.jsonl.sealing-0-source");
    let destination = root.join("turns.jsonl.zst");
    let outside_source = root.join("outside-source");
    let outside_destination = root.join("outside-destination");
    std::fs::write(&outside_source, b"outside source\n").unwrap();
    std::fs::write(&outside_destination, b"outside destination\n").unwrap();
    let source_before = std::fs::read(&outside_source).unwrap();
    let destination_before = std::fs::read(&outside_destination).unwrap();
    symlink(&outside_source, &source).unwrap();
    let generation = vaultr::vault::DetachedGeneration {
        path: source.clone(),
        base_len: 0,
        digest: vaultr::vault::sha256_file(&outside_source).unwrap(),
    };

    assert!(seal_generation(&generation, &destination).await.is_err());
    assert_eq!(std::fs::read(&outside_source).unwrap(), source_before);
    assert!(!destination.exists());

    std::fs::remove_file(&source).unwrap();
    std::fs::write(&source, b"detached evidence\n").unwrap();
    symlink(&outside_destination, &destination).unwrap();
    let generation = vaultr::vault::DetachedGeneration {
        path: source.clone(),
        base_len: 0,
        digest: vaultr::vault::sha256_file(&source).unwrap(),
    };
    assert!(seal_generation(&generation, &destination).await.is_err());
    assert_eq!(
        std::fs::read(&outside_destination).unwrap(),
        destination_before
    );
    assert_eq!(std::fs::read(&source).unwrap(), b"detached evidence\n");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn sealing_restart_cleans_exact_uuid_temps_and_rejects_near_misses() {
    if !which("zstd") {
        return;
    }
    let root =
        std::env::temp_dir().join(format!("plant-seal-temp-recovery-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let capture = detached(&root, "turns.jsonl", b"capture generation\n", 0);
    let destination = root.join("turns.jsonl.zst");
    let frame = root.join(format!(".turns.jsonl.zst.frame-{}", uuid::Uuid::new_v4()));
    let merged = root.join(format!(".turns.jsonl.zst.merged-{}", uuid::Uuid::new_v4()));
    std::fs::write(&frame, b"partial compressed debris").unwrap();
    std::fs::write(&merged, b"partial merged debris").unwrap();
    seal_generation(&capture, &destination).await.unwrap();
    assert!(!frame.exists());
    assert!(!merged.exists());

    let herdr = detached(&root, "herdr.jsonl", b"herdr generation\n", 0);
    let herdr_destination = root.join("herdr.jsonl.zst");
    let near_miss = root.join(".herdr.jsonl.zst.frame-not-a-canonical-v4-uuid");
    std::fs::write(&near_miss, b"unrecognized evidence").unwrap();
    assert!(seal_generation(&herdr, &herdr_destination).await.is_err());
    assert_eq!(std::fs::read(&near_miss).unwrap(), b"unrecognized evidence");
    assert!(herdr.path.exists());
    assert!(!herdr_destination.exists());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn legacy_temp_upgrade_uses_the_parent_with_extension_names() {
    if !which("zstd") {
        return;
    }
    assert_eq!(
        Path::new("herdr.jsonl").with_extension("frame-tmp"),
        Path::new("herdr.frame-tmp")
    );
    assert_eq!(
        Path::new("herdr.jsonl").with_extension("zst-tmp"),
        Path::new("herdr.zst-tmp")
    );

    let root = std::env::temp_dir().join(format!("plant-legacy-temp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let turns = root.join("turns.jsonl");
    std::fs::write(&turns, b"capture generation\n").unwrap();
    std::fs::write(root.join("turns.scrub-tmp"), b"partial scrub debris").unwrap();
    assert!(scrub(&turns).await);
    assert!(!root.join("turns.scrub-tmp").exists());

    let directory = SessionDirectory::open(&root).unwrap();
    directory.lock_exclusive().unwrap();
    let capture = detach_capture(&directory).unwrap();
    drop(directory);
    for name in ["turns.jsonl.frame-tmp", "turns.jsonl.zst-tmp"] {
        std::fs::write(root.join(name), b"partial capture seal debris").unwrap();
    }
    seal_generation(&capture, &root.join("turns.jsonl.zst"))
        .await
        .unwrap();

    let herdr = detached(&root, "herdr.jsonl", b"sidecar generation\n", 0);
    for name in ["herdr.frame-tmp", "herdr.zst-tmp"] {
        std::fs::write(root.join(name), b"partial sidecar seal debris").unwrap();
    }
    seal_generation(&herdr, &root.join("herdr.jsonl.zst"))
        .await
        .unwrap();
    for name in [
        "turns.jsonl.frame-tmp",
        "turns.jsonl.zst-tmp",
        "herdr.frame-tmp",
        "herdr.zst-tmp",
    ] {
        assert!(!root.join(name).exists(), "{name}");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn misclaimed_herdr_legacy_names_and_unsafe_entries_fail_closed() {
    use std::os::unix::fs::symlink;

    for suspect in [
        "herdr.jsonl.frame-tmp",
        "herdr.jsonl.zst-tmp",
        "herdr.frame-tm",
    ] {
        let root = std::env::temp_dir().join(format!(
            "plant-herdr-temp-near-miss-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let generation = detached(&root, "herdr.jsonl", b"sidecar evidence\n", 0);
        std::fs::write(root.join(suspect), b"unrecognized evidence").unwrap();
        assert!(
            seal_generation(&generation, &root.join("herdr.jsonl.zst"))
                .await
                .is_err(),
            "{suspect}"
        );
        assert!(generation.path.exists());
        assert_eq!(
            std::fs::read(root.join(suspect)).unwrap(),
            b"unrecognized evidence"
        );
        assert!(!root.join("herdr.jsonl.zst").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    for kind in ["symlink", "directory"] {
        let root =
            std::env::temp_dir().join(format!("plant-herdr-temp-{kind}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let generation = detached(&root, "herdr.jsonl", b"sidecar evidence\n", 0);
        let suspect = root.join("herdr.frame-tmp");
        let outside = root.with_extension(format!("{kind}-outside"));
        std::fs::write(&outside, b"outside evidence").unwrap();
        if kind == "symlink" {
            symlink(&outside, &suspect).unwrap();
        } else {
            std::fs::create_dir(&suspect).unwrap();
        }
        assert!(seal_generation(&generation, &root.join("herdr.jsonl.zst"))
            .await
            .is_err());
        assert!(std::fs::symlink_metadata(&suspect).is_ok());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside evidence");
        assert!(generation.path.exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(outside);
    }
}

#[tokio::test]
async fn corrupt_successful_compression_never_retires_detached_evidence() {
    let root =
        std::env::temp_dir().join(format!("plant-corrupt-compressor-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let generation = detached(&root, "turns.jsonl", b"sole detached evidence\n", 0);
    let destination = root.join("turns.jsonl.zst");
    let error = seal_generation_with(&generation, &destination, FrameCompressor::CorruptSuccess)
        .await
        .unwrap_err();
    assert!(error.contains(&destination.display().to_string()));
    assert!(!error.contains("sole detached evidence"));
    assert_eq!(
        std::fs::read(&generation.path).unwrap(),
        b"sole detached evidence\n"
    );
    assert!(destination.exists());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn sealing_retry_accepts_a_different_valid_frame_representation() {
    let root = std::env::temp_dir().join(format!("plant-valid-frame-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let base = zstd::encode_all("prior generation\n".as_bytes(), 7).unwrap();
    let generation = detached(
        &root,
        "turns.jsonl",
        b"detached generation\n",
        base.len() as u64,
    );
    let destination = root.join("turns.jsonl.zst");
    let mut committed = base;
    committed.extend(zstd::encode_all("detached generation\n".as_bytes(), 1).unwrap());
    std::fs::write(&destination, &committed).unwrap();
    File::open(&destination).unwrap().sync_all().unwrap();
    SessionDirectory::open(&root).unwrap().sync().unwrap();

    seal_generation(&generation, &destination).await.unwrap();
    assert!(!generation.path.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), committed);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn compressor_timeout_kills_and_reaps_the_child() {
    let mut child = tokio::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id().unwrap() as libc::pid_t;
    let error = wait_for_compressor(&mut child, Duration::from_millis(20))
        .await
        .unwrap_err();
    assert!(error.contains("killed and reaped"), "{error}");
    assert!(child.id().is_none());
    // SAFETY: signal 0 only probes the former pid.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[tokio::test]
async fn detached_sidecar_appends_frames_for_resumed_sessions() {
    if !which("zstd") {
        return;
    }
    let root = std::env::temp_dir().join(format!("plant-sidecar-seal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("herdr.jsonl.zst");

    for body in [b"generation-one\n".as_slice(), b"generation-two\n"] {
        std::fs::write(root.join("herdr.jsonl"), body).unwrap();
        let directory = SessionDirectory::open(&root).unwrap();
        directory.lock_exclusive().unwrap();
        let generation = detached_sidecar(&directory).unwrap().unwrap();
        drop(directory);
        seal_generation(&generation, &destination).await.unwrap();
    }
    assert_eq!(
        zstd::decode_all(File::open(&destination).unwrap()).unwrap(),
        b"generation-one\ngeneration-two\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn capture_retry_after_durable_destination_rename_is_exactly_once() {
    if !which("zstd") {
        return;
    }
    let root = std::env::temp_dir().join(format!("plant-capture-retry-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("turns.jsonl.zst");

    std::fs::write(root.join("turns.jsonl"), b"generation-one\n").unwrap();
    let directory = SessionDirectory::open(&root).unwrap();
    directory.lock_exclusive().unwrap();
    let first = detach_capture(&directory).unwrap();
    drop(directory);
    seal_generation(&first, &destination).await.unwrap();

    std::fs::write(root.join("turns.jsonl"), b"generation-two\n").unwrap();
    let directory = SessionDirectory::open(&root).unwrap();
    directory.lock_exclusive().unwrap();
    let second = detach_capture(&directory).unwrap();
    drop(directory);
    let frame = root.join("manual-frame.zst");
    let result = run(
        &[
            "zstd",
            "-19",
            "-T0",
            "-q",
            "-f",
            "-o",
            frame.to_str().unwrap(),
            second.path.to_str().unwrap(),
        ],
        Duration::from_secs(60),
    )
    .await;
    assert!(result.ok, "{}", result.failure_detail());
    let merged = root.join("manual-merged.zst");
    let mut output = File::create(&merged).unwrap();
    output
        .write_all(&std::fs::read(&destination).unwrap())
        .unwrap();
    output.write_all(&std::fs::read(&frame).unwrap()).unwrap();
    output.sync_all().unwrap();
    std::fs::rename(&merged, &destination).unwrap();
    SessionDirectory::open(&root).unwrap().sync().unwrap();

    seal_generation(&second, &destination).await.unwrap();
    assert!(!second.path.exists());
    assert_eq!(
        zstd::decode_all(File::open(&destination).unwrap()).unwrap(),
        b"generation-one\ngeneration-two\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn herdr_retry_after_durable_destination_rename_is_exactly_once() {
    if !which("zstd") {
        return;
    }
    let root = std::env::temp_dir().join(format!("plant-herdr-retry-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("herdr.jsonl.zst");

    std::fs::write(root.join("herdr.jsonl"), b"generation-one\n").unwrap();
    let directory = SessionDirectory::open(&root).unwrap();
    directory.lock_exclusive().unwrap();
    let first = detached_sidecar(&directory).unwrap().unwrap();
    drop(directory);
    seal_generation(&first, &destination).await.unwrap();

    std::fs::write(root.join("herdr.jsonl"), b"generation-two\n").unwrap();
    let directory = SessionDirectory::open(&root).unwrap();
    directory.lock_exclusive().unwrap();
    let second = detached_sidecar(&directory).unwrap().unwrap();
    drop(directory);
    let frame = root.join("manual-herdr-frame.zst");
    let result = run(
        &[
            "zstd",
            "-19",
            "-T0",
            "-q",
            "-f",
            "-o",
            frame.to_str().unwrap(),
            second.path.to_str().unwrap(),
        ],
        Duration::from_secs(60),
    )
    .await;
    assert!(result.ok, "{}", result.failure_detail());
    let merged = root.join("manual-herdr-merged.zst");
    let mut output = File::create(&merged).unwrap();
    output
        .write_all(&std::fs::read(&destination).unwrap())
        .unwrap();
    output.write_all(&std::fs::read(&frame).unwrap()).unwrap();
    output.sync_all().unwrap();
    std::fs::rename(&merged, &destination).unwrap();
    SessionDirectory::open(&root).unwrap().sync().unwrap();
    let committed = std::fs::read(&destination).unwrap();

    seal_generation(&second, &destination).await.unwrap();
    assert!(!second.path.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), committed);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn open_capture_journal_blocks_the_owned_sealing_transaction() {
    let root = std::env::temp_dir().join(format!("plant-seal-readiness-{}", uuid::Uuid::new_v4()));
    let vault = root.join("sessions");
    let sid = uuid::Uuid::new_v4().to_string();
    let directory = vault.join("2026/07/20").join(&sid);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("turns.jsonl"), b"capture evidence\n").unwrap();
    let canonical = std::fs::canonicalize(&vault).unwrap().display().to_string();
    let request_id = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        directory.join("state.json"),
        serde_json::json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": sid,
            "request_body": {},
            "capture_order": {
                "next_sequence": 1,
                "next_to_drain": 0,
                "pending": {
                    "0": {
                        "request_id": request_id,
                        "session_id": sid
                    }
                },
                "root": canonical
            }
        })
        .to_string(),
    )
    .unwrap();

    assert!(seal_ready_generation(&vault, &sid, &directory)
        .await
        .unwrap()
        .is_none());
    assert!(directory.join("turns.jsonl").exists());
    assert!(!std::fs::read_dir(&directory)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("turns.jsonl.sealing-"))));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn herdr_append_waits_behind_capture_owned_compression_detachment() {
    if !which("zstd") {
        return;
    }
    let root = std::env::temp_dir().join(format!("plant-seal-herdr-lock-{}", uuid::Uuid::new_v4()));
    let vault = root.join("sessions");
    let sid = uuid::Uuid::new_v4().to_string();
    let directory = vault.join("2026/07/20").join(&sid);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("turns.jsonl"), b"capture generation\n").unwrap();
    std::fs::write(
        directory.join("herdr.jsonl"),
        b"{\"ts\":\"old\",\"pane\":\"old\"}\n",
    )
    .unwrap();
    let root_identity = canonical_root(&vault);
    let lock = session_lock(&root_identity, &sid);
    let guard = lock.lock().await;

    let seal_vault = vault.clone();
    let seal_sid = sid.clone();
    let seal_directory = directory.clone();
    let sealing = tokio::spawn(async move {
        seal_ready_generation(&seal_vault, &seal_sid, &seal_directory).await
    });
    tokio::task::yield_now().await;
    let append_vault = vault.clone();
    let append_sid = sid.clone();
    let appending = tokio::spawn(async move {
        append_herdr_snapshot(
            &append_vault,
            &append_sid,
            "{\"pane\":\"new\"}",
            "{\"ts\":\"new\",\"pane\":\"new\"}",
        )
        .await
    });
    tokio::task::yield_now().await;
    drop(guard);

    assert!(sealing.await.unwrap().unwrap().is_some());
    assert!(appending.await.unwrap().unwrap());
    assert_eq!(
        zstd::decode_all(File::open(directory.join("herdr.jsonl.zst")).unwrap()).unwrap(),
        b"{\"ts\":\"old\",\"pane\":\"old\"}\n"
    );
    assert_eq!(
        std::fs::read(directory.join("herdr.jsonl")).unwrap(),
        b"{\"ts\":\"new\",\"pane\":\"new\"}\n"
    );
    let _ = std::fs::remove_dir_all(root);
}
