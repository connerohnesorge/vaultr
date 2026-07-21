use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "plant-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn free_ports() -> (u16, u16) {
    let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let second = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    (
        first.local_addr().unwrap().port(),
        second.local_addr().unwrap().port(),
    )
}

fn command(home: &Path, sessions: &Path, claude_port: u16, codex_port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_plant"));
    command
        .env("HOME", home)
        .env("VAULT_SESSIONS", sessions)
        .env("VAULTR_ANTHROPIC_PORT", claude_port.to_string())
        .env("VAULTR_CODEX_PORT", codex_port.to_string())
        .env("PLANT_JOBS", "0");
    command
}

fn health(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    if write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.contains("\"service\":\"plant\"")
}

fn wait_healthy(claude_port: u16, codex_port: u16) {
    for _ in 0..100 {
        if health(claude_port) && health(codex_port) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("plant did not become healthy on both ports");
}

fn recovery_fixture(sessions: &Path) -> PathBuf {
    let sid = "00000000-0000-4000-8000-000000000020";
    let dir = sessions.join("2026/07/20").join(sid);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("turns.jsonl"), "").unwrap();
    let root = fs::canonicalize(sessions).unwrap().display().to_string();
    let request = json!({
        "schema_version": 1,
        "request_id": "00000000-0000-4000-8000-000000000021",
        "session_id": sid,
        "request": {"body_delta": {"history": {"key": "messages", "prefix_length": 0, "append": []}}}
    });
    fs::write(
        dir.join("state.json"),
        json!({
            "schema_version": 1,
            "harness": "claude-code",
            "session_id": sid,
            "request_body": {},
            "capture_order": {
                "next_sequence": 1,
                "next_to_drain": 0,
                "pending": {"0": request},
                "root": root,
            }
        })
        .to_string(),
    )
    .unwrap();
    dir
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn capture_barrier(
    proxy_port: u16,
    upstream: TcpListener,
    sid: &str,
) -> (
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let upstream_thread = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut request = [0u8; 16 * 1024];
        let _ = stream.read(&mut request).unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let body = "data: {\"type\":\"message_stop\"}\n\n";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let sid = sid.to_string();
    let client_thread = std::thread::spawn(move || {
        let body = json!({
            "model": "m",
            "stream": true,
            "messages": [{"role": "user", "content": "append during ownership proof"}],
            "metadata": {"user_id": json!({"session_id": sid}).to_string()}
        })
        .to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
        write!(
            stream,
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    });
    let joined = std::thread::spawn(move || {
        upstream_thread.join().unwrap();
        client_thread.join().unwrap();
    });
    (ready_rx, release_tx, joined)
}

#[test]
fn losing_process_does_not_recover_an_incumbents_vault() {
    let root = temp_root("ownership-race");
    let home = root.join("home");
    let sessions = root.join("sessions");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let (claude_port, codex_port) = free_ports();

    let mut incumbent = command(&home, &sessions, claude_port, codex_port)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_healthy(claude_port, codex_port);
    let evidence = recovery_fixture(&sessions);
    let state_before = fs::read(evidence.join("state.json")).unwrap();

    let loser = command(&home, &sessions, claude_port, codex_port)
        .output()
        .unwrap();
    assert_eq!(
        loser.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&loser.stderr)
    );
    assert_eq!(
        fs::read_to_string(evidence.join("turns.jsonl")).unwrap(),
        ""
    );
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(evidence.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["capture_order"]["next_to_drain"], 0);
    assert_eq!(fs::read(evidence.join("state.json")).unwrap(), state_before);

    stop(&mut incumbent);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn partial_port_collision_fails_releases_listener_and_preserves_evidence() {
    let root = temp_root("partial-ownership");
    let home = root.join("home");
    let sessions = root.join("sessions");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let claude_probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let dummy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let claude_port = claude_probe.local_addr().unwrap().port();
    let codex_port = dummy.local_addr().unwrap().port();
    drop(claude_probe);
    let evidence = recovery_fixture(&sessions);
    let state_before = fs::read(evidence.join("state.json")).unwrap();

    let loser = command(&home, &sessions, claude_port, codex_port)
        .output()
        .unwrap();
    assert_eq!(
        loser.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&loser.stdout)
    );
    let rebound = TcpListener::bind(("127.0.0.1", claude_port))
        .expect("partial listener released before exit");
    drop(rebound);
    assert_eq!(
        fs::read_to_string(evidence.join("turns.jsonl")).unwrap(),
        ""
    );
    assert_eq!(fs::read(evidence.join("state.json")).unwrap(), state_before);

    drop(dummy);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn manual_compression_cannot_race_a_draining_daemon_capture_append() {
    let root = temp_root("manual-compress-ownership");
    let home = root.join("home");
    let sessions = root.join("sessions");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let (claude_port, codex_port) = free_ports();
    let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let mut incumbent = command(&home, &sessions, claude_port, codex_port)
        .env(
            "VAULTR_ANTHROPIC_UPSTREAM",
            format!("http://127.0.0.1:{upstream_port}"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_healthy(claude_port, codex_port);

    let sid = "00000000-0000-4000-8000-000000000032";
    let (ready, release, request) = capture_barrier(claude_port, upstream, sid);
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(Command::new("kill")
        .args(["-TERM", &incumbent.id().to_string()])
        .status()
        .unwrap()
        .success());
    std::thread::sleep(Duration::from_millis(100));
    let manual = command(&home, &sessions, claude_port, codex_port)
        .args(["compress", "once", "--idle", "0s"])
        .output()
        .unwrap();
    assert_eq!(
        manual.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&manual.stderr)
    );
    release.send(()).unwrap();
    request.join().unwrap();

    let mut captured = None;
    for _ in 0..100 {
        captured = vaultr::vault::walk_session_dirs(&sessions)
            .unwrap()
            .into_iter()
            .find(|(found, _)| found == sid)
            .map(|(_, path)| path);
        if captured
            .as_ref()
            .is_some_and(|path| path.join("turns.jsonl").is_file())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let capture = captured.expect("daemon persisted capture append");
    assert_eq!(
        fs::read_to_string(capture.join("turns.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(vaultr::vault::CaptureGenerations::load(&capture)
        .unwrap()
        .detached
        .is_none());

    stop(&mut incumbent);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn daemon_scheduler_runs_compression_in_process_and_records_conflicts() {
    if !Command::new("zstd")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("zstd not on PATH; skipping");
        return;
    }

    let root = temp_root("scheduled-compress-ownership");
    let home = root.join("home");
    let sessions = root.join("vault/sessions");
    let jobs = root.join("vault/jobs");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    let marker = root.join("script-ran");
    let script = jobs.join("compress.1s.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf ran > '{}'\nexit 0\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let session = sessions.join("2026/07/20/conflict");
    fs::create_dir_all(&session).unwrap();
    let detached_body = b"detached evidence\n";
    let detached = session.join(format!(
        "turns.jsonl.sealing-0-{}",
        vaultr::vault::sha256_hex(detached_body)
    ));
    fs::write(&detached, detached_body).unwrap();
    let sealed = session.join("turns.jsonl.zst");
    let sealed_before = zstd::encode_all("different evidence\n".as_bytes(), 3).unwrap();
    fs::write(&sealed, &sealed_before).unwrap();

    let (claude_port, codex_port) = free_ports();
    let mut daemon = command(&home, &sessions, claude_port, codex_port)
        .env("PLANT_JOBS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_healthy(claude_port, codex_port);

    let ledger = home.join(".local/state/plant/jobs/compress.jsonl");
    let mut record = None;
    for _ in 0..200 {
        record = fs::read_to_string(&ledger).ok();
        if record.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let record = record.expect("scheduled compression recorded an outcome");
    let last: serde_json::Value = serde_json::from_str(record.lines().last().unwrap()).unwrap();
    assert_eq!(last["outcome"], "failed");
    assert!(last["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("conflicts")));
    assert!(!marker.exists(), "compress cadence script was not spawned");
    assert_eq!(fs::read(&detached).unwrap(), detached_body);
    assert_eq!(fs::read(&sealed).unwrap(), sealed_before);

    stop(&mut daemon);
    fs::remove_dir_all(root).unwrap();
}
