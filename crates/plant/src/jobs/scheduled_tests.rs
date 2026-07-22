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
    let hanging_semaphore = tokio::sync::Semaphore::new(1);
    let started = std::time::Instant::now();
    assert_eq!(
        dispatch_scheduled(
            &hanging,
            &sessions,
            &hanging_semaphore,
            Duration::from_millis(100),
        )
        .await,
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
            format!("attempt {} has no durable final outcome", fence.id)
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
