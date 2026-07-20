use super::*;

fn env_append(prefix: u64, role: &str, content: &str) -> String {
    json!({
        "harness": "claude-code",
        "request": { "body_delta": { "history": {
            "key": "messages", "prefix_length": prefix,
            "append": [{ "role": role, "content": content }],
        }}},
    })
    .to_string()
}

#[test]
fn concatenated_record_recovers_every_envelope() {
    // The historical concurrent-write artifact: two complete Envelopes on one
    // physical record (`JSONJSON`) followed by a blank record.
    let a = env_append(0, "user", "a");
    let b = env_append(1, "user", "b");
    let raw = format!("{a}{b}\n\n");
    let r = reconstruct_reader(raw.as_bytes()).unwrap();
    assert_eq!(r.envelopes, 2, "both concatenated envelopes applied");
    assert_eq!(r.history_len, 2);
    assert_eq!(r.messages[0]["content"], "a");
    assert_eq!(r.messages[1]["content"], "b");
}

#[test]
fn whitespace_only_records_ignored() {
    let a = env_append(0, "user", "a");
    let raw = format!("\n   \n{a}\n\t\n");
    let r = reconstruct_reader(raw.as_bytes()).unwrap();
    assert_eq!(r.envelopes, 1);
    assert_eq!(r.history_len, 1);
}

#[test]
fn live_raw_ignores_one_unterminated_final_fragment() {
    let a = env_append(0, "user", "a");
    let raw = format!("{a}\n{{\"harness\":\"claude-code\",\"req"); // truncated tail, no newline
    let r = reconstruct_reader(raw.as_bytes()).unwrap();
    assert_eq!(r.envelopes, 1);
}

#[test]
fn sealed_segment_fails_on_malformed_trailing_content() {
    // A sealed (fully terminated) capture must not silently drop a broken tail.
    let a = env_append(0, "user", "a");
    let sealed_bytes = format!("{a}\n{{\"harness\":\"claude-code\",\"req\n"); // terminated junk
    let tmp = tempfile::TempDir::new().unwrap();
    let zst = tmp.path().join("turns.jsonl.zst");
    std::fs::write(&zst, zstd::encode_all(sealed_bytes.as_bytes(), 3).unwrap()).unwrap();
    let err = reconstruct(&zst).unwrap_err().to_string();
    assert!(err.contains("sealed"), "error names the segment: {err}");
    assert!(
        !err.contains("harness"),
        "error must not echo content: {err}"
    );
}

#[test]
fn terminated_junk_record_in_raw_fails() {
    // A non-final terminated record that can't form an Envelope is corruption.
    let a = env_append(0, "user", "a");
    let raw = format!("{a}\nnot json at all\n{}\n", env_append(1, "user", "b"));
    let err = reconstruct_reader(raw.as_bytes()).unwrap_err().to_string();
    assert!(err.contains("raw record 2"), "locates the record: {err}");
}

#[cfg(unix)]
#[test]
fn retained_snapshot_survives_sealed_replace_and_detached_unlink() {
    let root = tempfile::TempDir::new().unwrap();
    let first = format!("{}\n", env_append(0, "user", "first"));
    let second = format!("{}\n", env_append(1, "user", "second"));
    let first_frame = zstd::encode_all(first.as_bytes(), 3).unwrap();
    let second_frame = zstd::encode_all(second.as_bytes(), 3).unwrap();
    let sealed = root.path().join("turns.jsonl.zst");
    std::fs::write(&sealed, &first_frame).unwrap();
    let detached = root.path().join(format!(
        "turns.jsonl.sealing-{}-{}",
        first_frame.len(),
        crate::vault::sha256_hex(second.as_bytes())
    ));
    std::fs::write(&detached, second.as_bytes()).unwrap();
    let merged = root.path().join(".merged");
    let mut committed = first_frame;
    committed.extend(second_frame);
    std::fs::write(&merged, committed).unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
    let writer_root = root.path().to_path_buf();
    let writer_detached = detached.clone();
    let writer = std::thread::spawn(move || {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&writer_root)
            .unwrap();
        ready_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        // SAFETY: directory remains open and flock borrows its descriptor.
        assert_ne!(
            unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
            0,
            "writer acquired EX while reconstruction retained SH"
        );
        let error = std::io::Error::last_os_error().raw_os_error();
        assert!(
            error == Some(libc::EWOULDBLOCK) || error == Some(libc::EAGAIN),
            "unexpected flock error: {error:?}"
        );
        blocked_tx.send(()).unwrap();
        loop {
            // SAFETY: directory remains open and flock borrows its descriptor.
            if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted
            );
        }
        std::fs::rename(
            writer_root.join(".merged"),
            writer_root.join("turns.jsonl.zst"),
        )
        .unwrap();
        std::fs::remove_file(writer_detached).unwrap();
    });
    ready_rx.recv().unwrap();

    let snapshot = ReconstructionSnapshot::with_hook(root.path(), || {
        go_tx.send(()).unwrap();
        blocked_rx.recv().unwrap();
    })
    .unwrap();
    writer.join().unwrap();
    let retained = reconstruct_snapshot(snapshot).unwrap();

    assert_eq!(retained.envelopes, 2);
    assert_eq!(retained.messages[0]["content"], "first");
    assert_eq!(retained.messages[1]["content"], "second");
    let fresh = reconstruct(&sealed).unwrap();
    assert_eq!(fresh.messages, retained.messages);
    assert_eq!(fresh.envelopes, 2);
}

#[test]
fn retained_live_raw_reader_stops_at_its_snapshot_length() {
    use std::io::Write;

    let root = tempfile::TempDir::new().unwrap();
    let raw = root.path().join("turns.jsonl");
    let first = format!("{}\n", env_append(0, "user", "first"));
    let second = format!("{}\n", env_append(1, "user", "second"));
    std::fs::write(&raw, first).unwrap();

    let retained = reconstruct_canonical_with_hook(&raw, || {
        let mut file = OpenOptions::new().append(true).open(&raw).unwrap();
        file.write_all(second.as_bytes()).unwrap();
    })
    .unwrap();

    assert_eq!(retained.envelopes, 1);
    assert_eq!(retained.messages[0]["content"], "first");
    let fresh = reconstruct(&raw).unwrap();
    assert_eq!(fresh.envelopes, 2);
    assert_eq!(fresh.messages[1]["content"], "second");
}

#[cfg(unix)]
#[test]
fn snapshot_rejects_symlink_fifo_directory_and_duplicate_inode_generations() {
    use std::os::unix::fs::symlink;

    let root = tempfile::TempDir::new().unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, b"outside evidence\n").unwrap();
    let raw = root.path().join("turns.jsonl");
    symlink(&outside, &raw).unwrap();
    assert!(reconstruct(&raw).unwrap_err().to_string().contains("open"));
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside evidence\n");
    std::fs::remove_file(&raw).unwrap();

    let fifo_name = CString::new(raw.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fifo_name is a valid NUL-terminated pathname.
    assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
    assert!(reconstruct(&raw)
        .unwrap_err()
        .to_string()
        .contains("not a regular file"));
    std::fs::remove_file(&raw).unwrap();

    std::fs::create_dir(&raw).unwrap();
    assert!(reconstruct(&raw)
        .unwrap_err()
        .to_string()
        .contains("not a regular file"));
    std::fs::remove_dir(&raw).unwrap();

    let sealed = root.path().join("turns.jsonl.zst");
    let body = format!("{}\n", env_append(0, "user", "first"));
    std::fs::write(&sealed, zstd::encode_all(body.as_bytes(), 3).unwrap()).unwrap();
    std::fs::hard_link(&sealed, &raw).unwrap();
    assert!(reconstruct(&raw)
        .unwrap_err()
        .to_string()
        .contains("duplicate capture generation inode"));
}

/// Test-only inverse of `encode_delta`: replay set/remove over the prior
/// body and `apply_delta` over its history.
fn apply_body(prior: &Value, delta: &Value, history_key: &str) -> Value {
    let mut out = prior.as_object().cloned().unwrap_or_default();
    for (k, v) in delta["set"].as_object().unwrap() {
        out.insert(k.clone(), v.clone());
    }
    for k in delta["remove"].as_array().unwrap() {
        out.remove(k.as_str().unwrap());
    }
    let mut msgs: Vec<Value> = prior
        .get(history_key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut dict = HashMap::new();
    apply_delta(&delta["history"], &mut msgs, &mut dict);
    out.insert(history_key.to_string(), Value::Array(msgs));
    Value::Object(out)
}

fn msg(i: u64) -> Value {
    json!({ "role": if i.is_multiple_of(2) { "user" } else { "assistant" }, "content": format!("m{i}") })
}

#[test]
fn encode_apply_round_trip_property() {
    const BIG: &[&str] = &["tools", "system"];
    // Deterministic LCG-driven generator: histories that share prefixes,
    // grow, compact, and diverge; big/small fields that change or vanish.
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut rand = move |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let mut prior = json!({});
    for _ in 0..200 {
        let mut body = Map::new();
        body.insert("model".into(), json!(format!("m{}", rand(3))));
        if rand(4) > 0 {
            body.insert("tools".into(), json!([{ "name": format!("t{}", rand(2)) }]));
        }
        if rand(4) > 0 {
            body.insert("system".into(), json!(format!("sys{}", rand(2))));
        }
        if rand(3) > 0 {
            body.insert("temperature".into(), json!(rand(10)));
        }
        let prior_len = prior
            .get("messages")
            .and_then(Value::as_array)
            .map_or(0, Vec::len) as u64;
        let keep = rand(prior_len + 1); // 0..=prior_len shared prefix
        let grow = rand(4);
        let history: Vec<Value> = (0..keep)
            .chain((100 + keep)..(100 + keep + grow)) // diverging tail
            .map(msg)
            .collect();
        body.insert("messages".into(), Value::Array(history));
        let body = Value::Object(body);

        let delta = encode_delta(&prior, &body, "messages", BIG);
        assert_eq!(
            apply_body(&prior, &delta, "messages"),
            body,
            "prior={prior} body={body} delta={delta}"
        );
        prior = body;
    }
}

#[test]
fn encode_delta_round_trip_big_field_set_and_remove() {
    const BIG: &[&str] = &["tools", "system"];
    let prior = json!({
        "model": "m",
        "system": "sys",
        "tools": [{ "name": "t" }],
        "temperature": 0.5,
        "messages": [msg(0), msg(1)],
    });
    // Big field `tools` changes, `system` disappears, small field
    // `temperature` disappears; history compacts to a diverging singleton.
    let body = json!({
        "model": "m",
        "tools": [{ "name": "t2" }],
        "messages": [msg(7)],
    });
    let delta = encode_delta(&prior, &body, "messages", BIG);
    assert_eq!(delta["set"]["tools"], body["tools"]);
    assert!(delta["set"].get("system").is_none());
    let removed = delta["remove"].as_array().unwrap();
    assert!(removed.contains(&json!("system")));
    assert!(removed.contains(&json!("temperature")));
    assert_eq!(delta["history"]["prefix_length"], 0);
    assert_eq!(apply_body(&prior, &delta, "messages"), body);

    // Unchanged big field: absent from `set`, restored from prior on apply.
    let body2 = json!({
        "model": "m",
        "tools": [{ "name": "t" }],
        "messages": [msg(0), msg(1), msg(2)],
    });
    let delta2 = encode_delta(&prior, &body2, "messages", BIG);
    assert!(delta2["set"].get("tools").is_none());
    assert_eq!(delta2["history"]["prefix_length"], 2);
    // `system` was dropped, `tools` kept-but-deduped: apply must restore
    // exactly body2, tools included.
    assert_eq!(apply_body(&prior, &delta2, "messages"), body2);
}
