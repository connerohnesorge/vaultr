use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "current_thread")]
async fn capacity_wait_cancellation_leaves_no_attempt_fence() {
    let root =
        std::env::temp_dir().join(format!("plant-scheduled-cancel-{}", uuid::Uuid::new_v4()));
    let state = root.join("state");
    let sessions = root.join("vault/sessions");
    let _state = crate::state::use_test_dir(state.clone());
    let job = Job {
        name: "waiting".to_string(),
        path: root.join("waiting.1m.sh"),
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let semaphore = Arc::new(tokio::sync::Semaphore::new(0));
    let scheduled = tokio::spawn({
        let semaphore = semaphore.clone();
        async move { dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await }
    });

    tokio::task::yield_now().await;
    assert!(
        !state.join("job-attempts/waiting.json").exists(),
        "capacity waiting must remain outside the attempt fence"
    );
    scheduled.abort();
    assert!(scheduled.await.unwrap_err().is_cancelled());
    assert!(!state.join("job-attempts/waiting.json").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn cross_process_capacity_lease_is_bounded() {
    let root = std::env::temp_dir().join(format!("plant-worker-capacity-{}", uuid::Uuid::new_v4()));
    let state = root.join("state");
    let _state = crate::state::use_test_dir(state.clone());

    let first = try_acquire_worker_capacity(1).unwrap().unwrap();
    assert!(
        try_acquire_worker_capacity(1).unwrap().is_none(),
        "a second process cannot acquire the occupied capacity slot"
    );
    drop(first);
    let second = try_acquire_worker_capacity(1)
        .unwrap()
        .expect("the slot is reusable after the worker releases it");
    drop(second);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn worker_without_capacity_publishes_no_attempt_fence() {
    let root =
        std::env::temp_dir().join(format!("plant-worker-no-capacity-{}", uuid::Uuid::new_v4()));
    let state = root.join("state");
    let sessions = root.join("vault/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let job = Job {
        name: "waiting".to_string(),
        path: root.join("waiting.1m.sh"),
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let occupied = try_acquire_worker_capacity(1).unwrap().unwrap();

    assert_eq!(
        dispatch_scheduled_worker(&job, &sessions, 1, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Blocked
    );
    assert!(!state.join("job-attempts/waiting.json").exists());
    drop(occupied);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn same_job_workers_share_one_attempt_flock() {
    let root = std::env::temp_dir().join(format!("plant-worker-same-job-{}", uuid::Uuid::new_v4()));
    let state = root.join("state");
    let sessions = root.join("vault/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let job = Job {
        name: "same-job".to_string(),
        path: root.join("same-job.1m.sh"),
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let capacity = try_acquire_worker_capacity(2).unwrap().unwrap();
    let first = match begin_scheduled_attempt(&job).unwrap() {
        ScheduledAttemptStart::Ready(attempt) => attempt,
        ScheduledAttemptStart::NotDue | ScheduledAttemptStart::Blocked(_) => {
            panic!("the first worker must admit the due job")
        }
    };

    assert_eq!(
        dispatch_scheduled_worker(&job, &sessions, 2, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Blocked
    );
    assert!(!state.join("jobs/same-job.jsonl").exists());
    drop(first);
    drop(capacity);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_schedulers_execute_one_due_period() {
    let root = std::env::temp_dir().join(format!("plant-scheduled-race-{}", uuid::Uuid::new_v4()));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let calls = root.join("calls");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let _cwd = use_test_script_cwd(root.clone());
    let script = root.join("concurrent.1m.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho called >> '{}'\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let job = Job {
        name: "concurrent".to_string(),
        path: script,
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let first_capacity = tokio::sync::Semaphore::new(1);
    let second_capacity = tokio::sync::Semaphore::new(1);

    let _ = tokio::join!(
        dispatch_scheduled(&job, &sessions, &first_capacity, SCRIPT_BACKSTOP),
        dispatch_scheduled(&job, &sessions, &second_capacity, SCRIPT_BACKSTOP),
    );

    assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
    assert_eq!(
        std::fs::read_to_string(state.join("jobs/concurrent.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_holds_one_guard_across_capacity_and_typed_execution() {
    let root = std::env::temp_dir().join(format!("plant-scheduled-{}", uuid::Uuid::new_v4()));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let marker = root.join("wrapper-called");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let _cwd = use_test_script_cwd(root.clone());
    let wrapper = root.join("compress.30m.sh");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let job = Job {
        name: "compress".to_string(),
        path: wrapper,
        every: Duration::from_secs(1800),
        action: JobAction::InProcessCompression,
    };

    crate::capture::reset_recovery_calls();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(0));
    let scheduled = tokio::spawn({
        let job = job.clone();
        let sessions = sessions.clone();
        let semaphore = semaphore.clone();
        async move { dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await }
    });
    let fence_path = state.join("job-attempts/compress.json");
    tokio::task::yield_now().await;
    assert!(
        !fence_path.exists(),
        "capacity waiting must remain outside the attempt fence"
    );

    semaphore.add_permits(1);
    assert_eq!(scheduled.await.unwrap(), ScheduledDispatch::Finished(0));
    assert!(
        !marker.exists(),
        "scheduled compression must not run its wrapper"
    );
    assert_eq!(
        crate::capture::recovery_calls(),
        0,
        "scheduled compression must not run startup capture recovery"
    );
    let ledger_path = state.join("jobs/compress.jsonl");
    let ledger = std::fs::read_to_string(&ledger_path).unwrap();
    let record: serde_json::Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
    let attempt_id = record["attempt_id"].as_str().unwrap().to_string();
    assert_eq!(record["outcome"], "success");
    assert!(
        !fence_path.exists(),
        "the successful final record is durable before fence clearing"
    );

    write_fence(
        &fence_path,
        &AttemptFence {
            id: attempt_id,
            started_ts: epoch_now(),
            retryable: false,
            action: None,
        },
    )
    .unwrap();
    assert!(matches!(
        begin_scheduled_attempt(&job).unwrap(),
        ScheduledAttemptStart::NotDue
    ));
    assert!(
        !fence_path.exists(),
        "the durable old fence is reconciled before the due check"
    );

    let retry_path = root.join("retry.1m.sh");
    std::fs::write(&retry_path, "#!/bin/sh\nexit 75\n").unwrap();
    std::fs::set_permissions(&retry_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let retry = Job {
        name: "retry".to_string(),
        path: retry_path,
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let retry_semaphore = tokio::sync::Semaphore::new(1);
    assert_eq!(
        dispatch_scheduled(&retry, &sessions, &retry_semaphore, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Finished(75)
    );
    assert!(!state.join("jobs/retry.jsonl").exists());
    let retry_fence: AttemptFence =
        serde_json::from_slice(&std::fs::read(state.join("job-attempts/retry.json")).unwrap())
            .unwrap();
    assert!(
        retry_fence.retryable,
        "retryable execution returns 75 and retains a retryable fence"
    );

    let descendant_pid = root.join("descendant.pid");
    let hanging_path = root.join("pipe.1m.sh");
    std::fs::write(
        &hanging_path,
        format!(
            "#!/bin/sh\nsleep 10 &\necho $! > '{}'\nexit 0\n",
            descendant_pid.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hanging_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hanging = Job {
        name: "pipe".to_string(),
        path: hanging_path,
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let started = std::time::Instant::now();
    assert_eq!(
        dispatch_scheduled_worker(&hanging, &sessions, 1, Duration::from_millis(100),).await,
        ScheduledDispatch::Finished(1)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(std::fs::read_to_string(state.join("jobs/pipe.jsonl"))
        .unwrap()
        .contains("\"outcome\":\"failed\""));
    assert!(
        !state.join("job-attempts/pipe.json").exists(),
        "failed execution returns 1 and clears only after its durable record"
    );
    let descendant_pid = std::fs::read_to_string(&descendant_pid)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok());
    if let Some(pid) = descendant_pid {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn resumed_compression_uses_the_durable_action_inside_the_daemon() {
    let root = std::env::temp_dir().join(format!(
        "plant-resumed-compression-action-{}",
        uuid::Uuid::new_v4()
    ));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let marker = root.join("wrapper-called");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(state.join("job-attempts")).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let wrapper = root.join("compress.30m.sh");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_fence(
        &state.join("job-attempts/compress.json"),
        &AttemptFence {
            id: "resumed-under-listener-owner".to_string(),
            started_ts: 1,
            retryable: false,
            action: Some(JobAction::InProcessCompression),
        },
    )
    .unwrap();
    let job = Job {
        name: "compress".to_string(),
        path: wrapper,
        every: Duration::from_secs(1800),
        // The durable fence, not current discovery metadata, owns replay.
        action: JobAction::Script,
    };

    let semaphore = tokio::sync::Semaphore::new(1);
    assert_eq!(
        dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Finished(0)
    );
    assert!(!marker.exists(), "replay must stay in-process");
    let ledger = std::fs::read_to_string(state.join("jobs/compress.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = ledger
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["attempt_id"], "resumed-under-listener-owner");
    assert!(!state.join("job-attempts/compress.json").exists());

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn typed_script_fence_without_a_receipt_stays_blocked() {
    let root =
        std::env::temp_dir().join(format!("plant-typed-script-fence-{}", uuid::Uuid::new_v4()));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let marker = root.join("script-called");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(state.join("job-attempts")).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let script = root.join("audit.1m.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_fence(
        &state.join("job-attempts/audit.json"),
        &AttemptFence {
            id: "typed-script-unresolved".to_string(),
            started_ts: 1,
            retryable: false,
            action: Some(JobAction::Script),
        },
    )
    .unwrap();
    let job = Job {
        name: "audit".to_string(),
        path: script,
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };

    let semaphore = tokio::sync::Semaphore::new(1);
    assert_eq!(
        dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Blocked
    );
    assert!(!marker.exists(), "an unresolved script must not run twice");
    assert!(state.join("job-attempts/audit.json").exists());
    assert!(!state.join("jobs/audit.jsonl").exists());

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ledger_backed_fence_clears_without_execution() {
    let root = std::env::temp_dir().join(format!(
        "plant-ledger-backed-compression-{}",
        uuid::Uuid::new_v4()
    ));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let marker = root.join("wrapper-called");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(state.join("job-attempts")).unwrap();
    std::fs::create_dir_all(state.join("jobs")).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let wrapper = root.join("compress.30m.sh");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_fence(
        &state.join("job-attempts/compress.json"),
        &AttemptFence {
            id: "already-durable".to_string(),
            started_ts: 1,
            retryable: false,
            action: Some(JobAction::InProcessCompression),
        },
    )
    .unwrap();
    std::fs::write(
        state.join("jobs/compress.jsonl"),
        format!(
            "{{\"ts\":{},\"attempt_id\":\"already-durable\",\"outcome\":\"success\"}}\n",
            epoch_now()
        ),
    )
    .unwrap();
    let job = Job {
        name: "compress".to_string(),
        path: wrapper,
        every: Duration::from_secs(1800),
        action: JobAction::InProcessCompression,
    };

    let semaphore = tokio::sync::Semaphore::new(1);
    assert_eq!(
        dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::NotDue
    );
    assert!(!marker.exists(), "durable completion must not replay");
    assert!(!state.join("job-attempts/compress.json").exists());
    assert_eq!(
        std::fs::read_to_string(state.join("jobs/compress.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    std::fs::remove_dir_all(root).unwrap();
}

/// Write a keyed Agent Run receipt the way the `plant agent run` child would,
/// straight to its content-addressed path, so the scheduler side can be tested
/// without a Herdr lifecycle.
fn write_receipt(state: &Path, key: &str, body: &str) {
    use sha2::Digest;
    let dir = state.join("agent-runs");
    std::fs::create_dir_all(&dir).unwrap();
    let name = format!("{:x}.json", sha2::Sha256::digest(key.as_bytes()));
    std::fs::write(dir.join(name), body).unwrap();
}

fn strand_fence(state: &Path, name: &str, id: &str) {
    write_fence(
        &state.join(format!("job-attempts/{name}.json")),
        &AttemptFence {
            id: id.to_string(),
            started_ts: epoch_now(),
            retryable: false,
            action: None,
        },
    )
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn conclusive_receipts_reconcile_fences_the_scheduler_never_recorded() {
    let root = std::env::temp_dir().join(format!("plant-receipt-fence-{}", uuid::Uuid::new_v4()));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let attempt_ids = root.join("attempt-ids");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let _cwd = use_test_script_cwd(root.clone());
    let script = root.join("audit.1m.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$PLANT_ATTEMPT_ID\" >> '{}'\n",
            attempt_ids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let job = Job {
        name: "audit".to_string(),
        path: script,
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };

    // The script sees the published attempt ID, so it can key its agent run.
    assert_eq!(
        dispatch_scheduled_worker(&job, &sessions, 1, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Finished(0)
    );
    let ledger = std::fs::read_to_string(state.join("jobs/audit.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&attempt_ids).unwrap().trim(),
        record["attempt_id"].as_str().unwrap()
    );

    // A Plant restart between the completed agent run and its ledger record.
    for (id, body, outcome) in [
        (
            "succeeded-attempt",
            "{\"state\":\"succeeded\",\"key\":\"succeeded-attempt\",\"detail\":\"agent done\"}",
            "success",
        ),
        (
            "failed-attempt",
            "{\"state\":\"failed\",\"key\":\"failed-attempt\",\"detail\":\"agent failed\"}",
            "failed",
        ),
    ] {
        strand_fence(&state, "audit", id);
        write_receipt(&state, id, body);
        assert!(matches!(
            reconcile_fence("audit").unwrap(),
            FenceReconcile::Ready
        ));
        assert!(!state.join("job-attempts/audit.json").exists());
        let reconciled: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(state.join("jobs/audit.jsonl"))
                .unwrap()
                .lines()
                .next_back()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reconciled["attempt_id"], id);
        assert_eq!(reconciled["outcome"], outcome);
        assert!(reconciled["detail"]
            .as_str()
            .unwrap()
            .starts_with("reconciled from agent run receipt:"));
    }
    assert_eq!(
        std::fs::read_to_string(&attempt_ids)
            .unwrap()
            .lines()
            .count(),
        1,
        "reconciliation must not launch the agent again"
    );

    // Nonconclusive receipts keep the fence and keep the job undispatched.
    for (id, body) in [
        ("absent-attempt", None),
        (
            "pending-attempt",
            Some("{\"state\":\"in_progress\",\"key\":\"pending-attempt\"}"),
        ),
        ("corrupt-attempt", Some("{")),
        (
            "mismatched-attempt",
            Some("{\"state\":\"succeeded\",\"key\":\"other\",\"detail\":\"not mine\"}"),
        ),
    ] {
        strand_fence(&state, "audit", id);
        if let Some(body) = body {
            write_receipt(&state, id, body);
        }
        let before = std::fs::read_to_string(state.join("jobs/audit.jsonl")).unwrap();
        match reconcile_fence("audit").unwrap() {
            FenceReconcile::Blocked(detail) => assert!(detail.contains(id), "{detail}"),
            FenceReconcile::Ready | FenceReconcile::ResumableCompression(_) => {
                panic!("{id} must not clear its fence")
            }
        }
        assert!(state.join("job-attempts/audit.json").exists());
        assert_eq!(
            std::fs::read_to_string(state.join("jobs/audit.jsonl")).unwrap(),
            before
        );
        assert_eq!(
            dispatch_scheduled_worker(&job, &sessions, 1, SCRIPT_BACKSTOP).await,
            ScheduledDispatch::Blocked
        );
    }
    assert_eq!(
        std::fs::read_to_string(&attempt_ids)
            .unwrap()
            .lines()
            .count(),
        1
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_pending_identity_retains_the_attempt_fence() {
    let root = std::env::temp_dir().join(format!(
        "plant-pending-identity-fence-{}",
        uuid::Uuid::new_v4()
    ));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let job = Job {
        name: "identity".to_string(),
        path: root.join("identity.1m.sh"),
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    strand_fence(&state, "identity", "pending-identity");
    write_receipt(
        &state,
        "pending-identity",
        r#"{"state":"in_progress","key":"pending-identity","identity":{"workspace_id":"w1","pane_id":"w1:p1","terminal_id":"t1","stage":"working"}}"#,
    );

    assert_eq!(
        dispatch_scheduled_worker(&job, &sessions, 1, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Blocked
    );
    assert!(state.join("job-attempts/identity.json").exists());
    assert!(!state.join("jobs/identity.jsonl").exists());

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_record_failure_blocks_the_next_dispatch() {
    let root = std::env::temp_dir().join(format!(
        "plant-scheduled-record-failure-{}",
        uuid::Uuid::new_v4()
    ));
    let sessions = root.join("vault/sessions");
    let state = root.join("state");
    let calls = root.join("calls");
    std::fs::create_dir_all(&sessions).unwrap();
    let _state = crate::state::use_test_dir(state.clone());
    let _cwd = use_test_script_cwd(root.clone());
    let script = root.join("record-fails.1m.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho called >> '{}'\nrmdir '{}'\necho unavailable > '{}'\n",
            calls.display(),
            state.join("jobs").display(),
            state.join("jobs").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let job = Job {
        name: "record-fails".to_string(),
        path: script,
        every: Duration::from_secs(60),
        action: JobAction::Script,
    };
    let semaphore = tokio::sync::Semaphore::new(1);

    assert_eq!(
        dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Finished(1)
    );
    std::fs::remove_file(state.join("jobs")).unwrap();
    std::fs::create_dir_all(state.join("jobs")).unwrap();
    let fence: AttemptFence = serde_json::from_slice(
        &std::fs::read(state.join("job-attempts/record-fails.json")).unwrap(),
    )
    .unwrap();
    assert!(!fence.retryable);
    match begin_scheduled_attempt(&job).unwrap() {
        ScheduledAttemptStart::Blocked(detail) => assert_eq!(
            detail,
            format!(
                "attempt {} has no durable final outcome; \
                 if it is abandoned, run `plant jobs unblock record-fails`",
                fence.id
            )
        ),
        _ => panic!("retained attempt fence must block the next scheduler cycle"),
    }
    assert_eq!(
        dispatch_scheduled(&job, &sessions, &semaphore, SCRIPT_BACKSTOP).await,
        ScheduledDispatch::Blocked
    );
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap().lines().count(),
        1,
        "the failed final record remains fenced across scheduler cycles"
    );
    assert!(state.join("job-attempts/record-fails.json").exists());

    std::fs::remove_dir_all(root).unwrap();
}
