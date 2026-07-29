use super::*;

fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("plant-{label}-{}", uuid::Uuid::new_v4()))
}

#[test]
fn health_job_alerts_on_low_headroom_and_on_each_dropped_turn_session() {
    let vault = temp_root("alerts").join("sessions");
    let meta = vault.join(".meta");
    std::fs::create_dir_all(&meta).unwrap();
    std::fs::write(meta.join("aaa.json"), r#"{"dropped_turns":3}"#).unwrap();
    std::fs::write(meta.join("bbb.json"), r#"{"dropped_turns":0}"#).unwrap();

    assert_eq!(
        dropped_turn_alerts(&vault),
        vec!["dropped-turn alert: aaa dropped=3".to_string()]
    );
    // A real temp volume has headroom; the alert fires only under the threshold.
    let alert = headroom_alert(&vault);
    let free = crate::fsutil::free_bytes(&vault).unwrap();
    assert_eq!(
        alert.is_some(),
        free < crate::fsutil::headroom_floor() * 2,
        "alert must track free={free}"
    );
    assert!(headroom_alert(&vault.join("no-such-volume")).is_none());
    std::fs::remove_dir_all(vault.parent().unwrap()).ok();
}

#[test]
fn eligibility_is_independent_per_learner() {
    let root = temp_root("sweep");
    let sessions = root.join("sessions");
    let claude_id = "claude-processed";
    let codex_id = "codex-processed";
    let claude_dir = sessions.join("2026/07/15").join(claude_id);
    let codex_dir = sessions.join("2026/07/15").join(codex_id);
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(claude_dir.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
    std::fs::write(codex_dir.join("turns.jsonl.zst"), "sealed").unwrap();
    std::fs::create_dir_all(root.join("learnings")).unwrap();
    std::fs::write(
        root.join("learnings/.ledger.jsonl"),
        format!(
            "{{\"session_id\":\"{claude_id}\"}}\n\
             {{\"session_id\":\"{codex_id}\",\"learner\":\"codex\"}}\n"
        ),
    )
    .unwrap();

    let claude = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::ClaudeCode).unwrap();
    let codex = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::Codex).unwrap();
    assert!(claude.iter().any(|path| path.ends_with(codex_id)));
    assert!(!claude.iter().any(|path| path.ends_with(claude_id)));
    assert!(codex.iter().any(|path| path.ends_with(claude_id)));
    assert!(!codex.iter().any(|path| path.ends_with(codex_id)));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn walker_skips_non_date_dirs_but_finds_dated_sessions() {
    let root = temp_root("walker");
    let sessions = root.join("sessions");
    let sid = "abc123-real-session";
    let dated = sessions.join("2026/07/16").join(sid);
    std::fs::create_dir_all(&dated).unwrap();
    std::fs::write(dated.join("turns.jsonl"), "{}\n").unwrap();
    std::fs::write(dated.join("turns.jsonl.zst"), "prior seal").unwrap();
    for bogus in [
        ".meta/2026/07",
        "notes/07/16",
        "2026/backup/16",
        "2026/07/.tmp",
    ] {
        let directory = sessions.join(bogus).join("phantom-session");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("turns.jsonl"), "{}\n").unwrap();
    }

    let (raw, _) = pending_generations(&sessions).unwrap();
    assert_eq!(raw.len(), 1, "only the dated session, got {raw:?}");
    assert_eq!(raw[0].sid, sid);
    assert_eq!(raw[0].selected, GenerationKind::Raw);
    assert_eq!(raw[0].path(), dated.join("turns.jsonl"));
    assert_eq!(
        raw[0].inventory.sealed.as_deref(),
        Some(dated.join("turns.jsonl.zst").as_path())
    );

    std::fs::remove_file(dated.join("turns.jsonl")).unwrap();
    let (all, _) = current_generations(&sessions).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].selected, GenerationKind::Sealed);
    assert_eq!(all[0].path(), dated.join("turns.jsonl.zst"));
    assert!(pending_generations(&sessions).unwrap().0.is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn walker_propagates_hostile_numeric_symlink_errors() {
    use std::os::unix::fs::symlink;

    let root = temp_root("walker-symlink");
    let sessions = root.join("sessions");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    symlink(&outside, sessions.join("2026")).unwrap();

    let error = current_generations(&sessions).unwrap_err();
    assert!(error.contains("symlink"), "{error}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn generation_policy_propagates_post_inventory_io_errors() {
    let root = temp_root("generation-io");
    let directory = root.join("session");
    let raw = directory.join("turns.jsonl");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(&raw, [0xff]).unwrap();
    let inventory = vaultr::vault::CaptureGenerations::load(&directory).unwrap();
    let generation = SessionGeneration::current("session".to_string(), inventory).unwrap();

    let error = generation.substantive().unwrap_err();
    assert!(error.contains(&raw.display().to_string()), "{error}");
    std::fs::remove_file(&raw).unwrap();
    let error = generation.idle_secs().unwrap_err();
    assert!(error.contains(&raw.display().to_string()), "{error}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn prior_generation_loss_after_inventory_fails_maintenance_policy() {
    let root = temp_root("prior-generation-io");
    let directory = root.join("session");
    let raw = directory.join("turns.jsonl");
    let sealed = directory.join("turns.jsonl.zst");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(&raw, "{}\n".repeat(6)).unwrap();
    std::fs::write(&sealed, "sealed").unwrap();
    let inventory = vaultr::vault::CaptureGenerations::load(&directory).unwrap();
    let generation = SessionGeneration::current("session".to_string(), inventory).unwrap();
    let learned = HashMap::from([("session".to_string(), u64::MAX)]);

    std::fs::remove_file(&sealed).unwrap();
    let error = generation.learned_current(&learned).unwrap_err();
    assert!(error.contains(&sealed.display().to_string()), "{error}");
    let error = generation
        .ready_to_seal(&learned, &learned, &HashSet::new(), Duration::ZERO)
        .unwrap_err();
    assert!(error.contains(&sealed.display().to_string()), "{error}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stuck_classification_covers_every_ledger_state() {
    let root = temp_root("stuck");
    let sessions = root.join("sessions");
    let day = sessions.join("2026/07/16");
    for (sid, body) in [
        ("both-ledgered", "{}\n".repeat(6)),
        ("claude-only", "{}\n".repeat(6)),
        ("codex-only", "{}\n".repeat(6)),
        ("nobody-big", "{}\n".repeat(6)),
        ("nobody-small", "{}\n".to_string()),
    ] {
        let directory = day.join(sid);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("turns.jsonl"), body).unwrap();
    }
    let sealed = day.join("already-sealed");
    std::fs::create_dir_all(&sealed).unwrap();
    std::fs::write(sealed.join("turns.jsonl.zst"), "sealed").unwrap();
    std::fs::create_dir_all(root.join("learnings")).unwrap();
    std::fs::write(
        root.join("learnings/.ledger.jsonl"),
        concat!(
            "{\"session_id\":\"both-ledgered\"}\n",
            "{\"session_id\":\"both-ledgered\",\"learner\":\"codex\"}\n",
            "{\"session_id\":\"claude-only\",\"learner\":\"claude\"}\n",
            "{\"session_id\":\"codex-only\",\"learner\":\"codex\"}\n",
        ),
    )
    .unwrap();

    let stuck = stuck_captures(&sessions, Duration::ZERO).unwrap();
    let state = |sid: &str| {
        stuck
            .iter()
            .find(|capture| capture.sid == sid)
            .map(|capture| capture.state)
            .unwrap_or_else(|| panic!("{sid} missing"))
    };
    assert_eq!(state("both-ledgered"), StuckState::SealBlocked);
    assert_eq!(
        state("claude-only"),
        StuckState::HalfLearned(Harness::Codex)
    );
    assert_eq!(
        state("codex-only"),
        StuckState::HalfLearned(Harness::ClaudeCode)
    );
    assert_eq!(state("nobody-big"), StuckState::Unlearned);
    assert_eq!(state("nobody-small"), StuckState::SubThreshold);
    assert!(!stuck.iter().any(|capture| capture.sid == "already-sealed"));
    assert!(stuck_captures(&sessions, Duration::from_secs(3600))
        .unwrap()
        .is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn inflight_lease_atomically_dedupes_dispatch_until_expiry() {
    let root = temp_root("inflight");
    let sessions = root.join("sessions");
    let sid = "e768f4c4-inflight";
    let directory = sessions.join("2026/07/16").join(sid);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
    std::fs::create_dir_all(root.join("learnings")).unwrap();

    let first = eligible_and_claim(
        &sessions,
        Duration::ZERO,
        10,
        Harness::ClaudeCode,
        Duration::from_secs(3600),
    )
    .unwrap();
    assert!(first.iter().any(|path| path.ends_with(sid)));

    let during = eligible_and_claim(
        &sessions,
        Duration::ZERO,
        10,
        Harness::ClaudeCode,
        Duration::from_secs(3600),
    )
    .unwrap();
    assert!(during.is_empty(), "active batch must win");

    publish_inflight(
        &inflight_path(&sessions, Harness::ClaudeCode).unwrap(),
        &InflightLease {
            sids: vec![sid.to_string()],
            expires_at: epoch_now().saturating_sub(1),
        },
    )
    .unwrap();
    let after = eligible_and_claim(
        &sessions,
        Duration::ZERO,
        10,
        Harness::ClaudeCode,
        Duration::from_secs(3600),
    )
    .unwrap();
    assert!(after.iter().any(|path| path.ends_with(sid)));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_active_lease_fails_closed() {
    let root = temp_root("inflight-malformed");
    let sessions = root.join("sessions");
    let learnings = root.join("learnings");
    std::fs::create_dir_all(&learnings).unwrap();
    let path = inflight_path(&sessions, Harness::ClaudeCode).unwrap();
    std::fs::write(&path, "{not-json").unwrap();

    let error = eligible_and_claim(
        &sessions,
        Duration::ZERO,
        10,
        Harness::ClaudeCode,
        Duration::from_secs(3600),
    )
    .unwrap_err();
    assert!(error.contains("parse"), "{error}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resumed_session_is_relearned_per_generation() {
    let root = temp_root("resume");
    let sessions = root.join("sessions");
    let sid = "resumed-after-seal";
    let directory = sessions.join("2026/07/14").join(sid);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
    std::fs::write(directory.join("turns.jsonl.zst"), "gen1-sealed").unwrap();
    let sealed_at = 1_784_050_961u64;
    std::fs::OpenOptions::new()
        .append(true)
        .open(directory.join("turns.jsonl.zst"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(sealed_at))
        .unwrap();
    std::fs::create_dir_all(root.join("learnings")).unwrap();
    std::fs::write(
        root.join("learnings/.ledger.jsonl"),
        format!(
            concat!(
                "{{\"session_id\":\"{}\",\"processed_at\":\"2026-07-14T15:08:26Z\"}}\n",
                "{{\"session_id\":\"{}\",\"learner\":\"codex\",",
                "\"processed_at\":\"2026-07-16T18:53:27Z\"}}\n"
            ),
            sid, sid
        ),
    )
    .unwrap();

    let stuck = stuck_captures(&sessions, Duration::ZERO).unwrap();
    let entry = stuck
        .iter()
        .find(|capture| capture.sid == sid)
        .expect("reported stuck");
    assert_eq!(entry.state, StuckState::HalfLearned(Harness::ClaudeCode));

    let claude = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::ClaudeCode).unwrap();
    let codex = eligible_sessions(&sessions, Duration::ZERO, 10, Harness::Codex).unwrap();
    assert!(claude.iter().any(|path| path.ends_with(sid)));
    assert!(!codex.iter().any(|path| path.ends_with(sid)));

    std::fs::remove_file(directory.join("turns.jsonl")).unwrap();
    let inventory = vaultr::vault::CaptureGenerations::load(&directory).unwrap();
    let sealed = SessionGeneration::current(sid.to_string(), inventory).unwrap();
    assert_eq!(sealed.selected, GenerationKind::Sealed);
    assert!(sealed
        .learned_current(
            &current_generations(&sessions)
                .unwrap()
                .1
                .latest(Harness::ClaudeCode)
        )
        .unwrap());

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn detached_sealing_conflict_is_an_operational_failure() {
    if !which("zstd") {
        return;
    }
    let root = temp_root("detached-conflict");
    let vault = root.join("sessions");
    let directory = vault.join("2026/07/20/conflict");
    std::fs::create_dir_all(&directory).unwrap();
    let body = b"detached evidence\n";
    let detached = directory.join(format!(
        "turns.jsonl.sealing-0-{}",
        vaultr::vault::sha256_hex(body)
    ));
    std::fs::write(&detached, body).unwrap();
    let sealed = directory.join("turns.jsonl.zst");
    let conflict = zstd::encode_all("different generation\n".as_bytes(), 3).unwrap();
    std::fs::write(&sealed, &conflict).unwrap();

    let error = compress_sweep(&vault, Duration::ZERO).await.unwrap_err();
    assert!(
        matches!(&error, CompressError::Operational(message) if message.contains("seal detached generation")),
        "{error}"
    );
    assert!(detached.exists(), "detached evidence is preserved");
    assert_eq!(std::fs::read(&sealed).unwrap(), conflict);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn compression_inventory_failures_are_distinct() {
    let missing = temp_root("compress-missing");
    let error = compress_sweep(&missing, Duration::ZERO).await.unwrap_err();
    assert!(
        matches!(&error, CompressError::Inventory(message) if message.contains(&missing.display().to_string())),
        "{error}"
    );
}

#[test]
fn job_sid_registry_parses_lines() {
    let path = temp_root("job-sids");
    std::fs::write(&path, "aaa-111\n\n  bbb-222  \n").unwrap();
    let sids = job_sids_at(&path);
    assert!(sids.contains("aaa-111") && sids.contains("bbb-222"));
    assert_eq!(sids.len(), 2);
    assert!(job_sids_at(Path::new("/nonexistent/registry")).is_empty());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn oversized_seal_is_gitignored_idempotently() {
    let root = temp_root("commit-cap");
    let sessions = root.join("sessions");
    let directory = sessions.join("2026/07/18/big-one");
    std::fs::create_dir_all(&directory).unwrap();
    let sealed = directory.join("turns.jsonl.zst");
    std::fs::write(&sealed, "blob").unwrap();

    exclude_from_commit(&sessions, &sealed, 2_700_000_000);
    exclude_from_commit(&sessions, &sealed, 2_700_000_000);
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    let line = "sessions/2026/07/18/big-one/turns.jsonl.zst";
    assert_eq!(gitignore.matches(line).count(), 1);
    assert!(gitignore.contains("2.7GB"));
    assert!(sealed.is_file());

    std::fs::remove_dir_all(root).unwrap();
}
