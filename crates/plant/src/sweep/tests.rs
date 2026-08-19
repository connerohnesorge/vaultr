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
fn quota_probe_filter_requires_both_state_fields_and_one_envelope() {
    let root = temp_root("quota-probes");
    let sessions = root.join("sessions");
    let day = sessions.join("2026/07/15");
    let standalone = day.join("standalone-quota-probe");
    let trailing = day.join("real-session-with-trailing-probe");
    let quota_prompt = day.join("real-session-mentioning-quota");

    for (directory, max_tokens) in [(&standalone, 1), (&trailing, 1), (&quota_prompt, 2)] {
        std::fs::create_dir_all(directory).unwrap();
        let state = serde_json::json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": directory.file_name().unwrap().to_str().unwrap(),
            "request_body": {
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": "quota"}]
            }
        });
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }
    std::fs::write(
        standalone.join("turns.jsonl.zst"),
        zstd::encode_all("{}\n".as_bytes(), 1).unwrap(),
    )
    .unwrap();
    std::fs::write(
        trailing.join("turns.jsonl.zst"),
        zstd::encode_all("{}\n{}\n".as_bytes(), 1).unwrap(),
    )
    .unwrap();
    // Invalid compressed bytes prove a non-candidate transcript is never decompressed.
    std::fs::write(quota_prompt.join("turns.jsonl.zst"), "not zstd").unwrap();

    for learner in [Harness::ClaudeCode, Harness::Codex] {
        let eligible = eligible_sessions(&sessions, Duration::ZERO, 10, learner).unwrap();
        assert!(!eligible.contains(&standalone), "{learner:?}: {eligible:?}");
        assert!(eligible.contains(&trailing), "{learner:?}: {eligible:?}");
        assert!(
            eligible.contains(&quota_prompt),
            "{learner:?}: {eligible:?}"
        );
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn quota_probe_confirmation_stops_after_two_envelopes_in_a_large_transcript() {
    use std::cell::Cell;
    use std::io::Read;
    use std::rc::Rc;

    struct EnvelopeReader {
        envelopes: Vec<Vec<u8>>,
        next: usize,
        reads: Rc<Cell<usize>>,
    }

    impl Read for EnvelopeReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let Some(envelope) = self.envelopes.get(self.next) else {
                return Ok(0);
            };
            assert!(
                envelope.len() <= buffer.len(),
                "confirmation requested the large third envelope"
            );
            buffer[..envelope.len()].copy_from_slice(envelope);
            self.next += 1;
            self.reads.set(self.reads.get() + 1);
            Ok(envelope.len())
        }
    }

    let reads = Rc::new(Cell::new(0));
    let reader = EnvelopeReader {
        envelopes: vec![
            b"{}\n".to_vec(),
            b"{}\n".to_vec(),
            vec![b'x'; 2 * 1024 * 1024],
        ],
        next: 0,
        reads: Rc::clone(&reads),
    };

    assert!(!transcript_has_exactly_one_envelope(reader).unwrap());
    assert_eq!(reads.get(), 2);
}

#[test]
fn measured_quota_probe_false_positive_2a82e018_remains_eligible() {
    let root = temp_root("quota-probe-false-positive");
    let sessions = root.join("sessions");
    let directory = sessions
        .join("2026/07/15")
        .join("2a82e018-3268-44f9-bda1-56be8b9bc9a9");
    std::fs::create_dir_all(&directory).unwrap();
    let state = serde_json::json!({
        "schema_version": 1,
        "harness": "claude-code",
        "session_id": "2a82e018-3268-44f9-bda1-56be8b9bc9a9",
        "request_body": {
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "quota"}]
        }
    });
    std::fs::write(
        directory.join("state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let transcript = "{}\n".repeat(369);
    std::fs::write(
        directory.join("turns.jsonl.zst"),
        zstd::encode_all(transcript.as_bytes(), 1).unwrap(),
    )
    .unwrap();

    for learner in [Harness::ClaudeCode, Harness::Codex] {
        let eligible = eligible_sessions(&sessions, Duration::ZERO, 10, learner).unwrap();
        assert_eq!(eligible, vec![directory.clone()]);
    }

    std::fs::remove_dir_all(root).unwrap();
}

/// A seal's mtime on any machine but the producer is when git wrote the file,
/// so a fresh clone would otherwise be blind to its entire corpus for one idle
/// window — and blind again after any re-checkout. Both files here are written
/// now; only the raw one may be held back.
#[test]
fn a_just_written_seal_is_eligible_but_a_just_written_raw_capture_is_not() {
    let root = temp_root("checkout-mtime");
    let sessions = root.join("sessions");
    let sealed_id = "sealed-by-another-machine";
    let raw_id = "still-being-written";
    let sealed_dir = sessions.join("2026/07/31").join(sealed_id);
    let raw_dir = sessions.join("2026/07/31").join(raw_id);
    std::fs::create_dir_all(&sealed_dir).unwrap();
    std::fs::create_dir_all(&raw_dir).unwrap();
    std::fs::write(sealed_dir.join("turns.jsonl.zst"), "sealed").unwrap();
    std::fs::write(raw_dir.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();

    let eligible = eligible_sessions(
        &sessions,
        Duration::from_secs(3600),
        10,
        Harness::ClaudeCode,
    )
    .unwrap();
    assert!(
        eligible.iter().any(|path| path.ends_with(sealed_id)),
        "a seal checked out seconds ago must still be learnable: {eligible:?}"
    );
    assert!(
        !eligible.iter().any(|path| path.ends_with(raw_id)),
        "a raw capture written seconds ago is still live and must wait: {eligible:?}"
    );

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
fn sealing_gates_on_idle_alone_not_on_the_learners() {
    let root = temp_root("seal-gate");
    let directory = root.join("session");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
    let inventory = vaultr::vault::CaptureGenerations::load(&directory).unwrap();
    let generation = SessionGeneration::current("session".to_string(), inventory).unwrap();

    // Nothing has learned this session and it is in no job registry — under the old
    // learned-both conjunct this could never seal, which is what stranded the backlog.
    assert!(generation.ready_to_seal(Duration::ZERO).unwrap());
    // Idle is still the whole gate: a capture written moments ago must not seal.
    assert!(!generation.ready_to_seal(Duration::from_secs(3600)).unwrap());

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
    // `learned_current` still guards the *eligibility* path (`eligible_candidates`,
    // the in-flight lease), so a vanished prior generation is still an error there.
    // `ready_to_seal` no longer consults it — sealing gates on idle alone — so it is
    // deliberately no longer asserted here.
    let error = generation.learned_current(&learned).unwrap_err();
    assert!(error.contains(&sealed.display().to_string()), "{error}");
    assert!(generation.ready_to_seal(Duration::ZERO).unwrap());

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
fn inflight_lease_dedupes_until_completion_or_expiry() {
    let root = temp_root("inflight");
    let sessions = root.join("sessions");
    let sid = "e768f4c4-inflight";
    let next_sid = "f87905d5-next";
    let directory = sessions.join("2026/07/16").join(sid);
    let next_directory = sessions.join("2026/07/16").join(next_sid);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::create_dir_all(&next_directory).unwrap();
    std::fs::write(directory.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
    std::fs::write(next_directory.join("turns.jsonl"), "{}\n".repeat(6)).unwrap();
    std::fs::create_dir_all(root.join("learnings")).unwrap();

    let first = eligible_and_claim(
        &sessions,
        Duration::ZERO,
        1,
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

    std::fs::write(
        directory.join("learn-claude-test-20260716T120000Z.json"),
        r#"{"processed_at":"2026-07-16T12:00:00Z","outcome":"learned"}"#,
    )
    .unwrap();
    let completed = eligible_and_claim(
        &sessions,
        Duration::ZERO,
        1,
        Harness::ClaudeCode,
        Duration::from_secs(3600),
    )
    .unwrap();
    assert!(
        completed.iter().any(|path| path.ends_with(next_sid)),
        "recorded batch must release the next claim before lease expiry"
    );

    publish_inflight(
        &inflight_path(&sessions, Harness::ClaudeCode).unwrap(),
        &InflightLease {
            sids: vec![next_sid.to_string()],
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
    assert!(after.iter().any(|path| path.ends_with(next_sid)));

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
        vaultr::digest::sha256_hex(body)
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
