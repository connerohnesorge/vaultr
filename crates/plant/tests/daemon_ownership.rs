use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
            "session_id": sid,
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

    drop(dummy);
    fs::remove_dir_all(root).unwrap();
}
