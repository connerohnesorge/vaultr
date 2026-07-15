//! --self-test: port of wireproxy.ts selfTest() (ts:724-848).
//! Fake upstream + fake OTLP, both adapters, zstd request, delta/compaction
//! assertions, header allowlist, 502 path, OTLP payload assertions, scrub.

use crate::adapter::adapters;
use crate::capture::session_dir;
use crate::proxy::{self, ProxyCtx};
use crate::sweep;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

const SSE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2}}}\n\n",
    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n",
);

/// fake upstream needs the request body consumed (hyper requires it); collect then respond
async fn serve_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let svc = hyper::service::service_fn(|req: Request<Incoming>| async move {
                    let _ = req.into_body().collect().await;
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header("content-type", "text/event-stream")
                            .header("content-encoding", "zstd")
                            .header("request-id", "req_test")
                            .header("x-secret", "drop-me")
                            .body(Full::new(Bytes::from(
                                zstd::encode_all(SSE.as_bytes(), 1).unwrap(),
                            )))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    port
}

fn start_proxy(
    mut adapter: crate::adapter::Adapter,
    upstream: String,
    vault: &std::path::Path,
    otel: Arc<crate::otel::Otel>,
) -> u16 {
    adapter.upstream = upstream;
    let (tx, rx) = std::sync::mpsc::channel();
    let vault = vault.to_path_buf();
    tokio::spawn(async move {
        let (listener, port) = proxy::bind(0).await.unwrap();
        tx.send(port).unwrap();
        let ctx = Arc::new(ProxyCtx {
            adapter,
            vault,
            client: crate::http_client(),
            otel,
        });
        proxy::serve(listener, ctx).await;
    });
    rx.recv().unwrap()
}

pub async fn self_test() {
    let vault_dir = std::env::temp_dir().join(format!("plant-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&vault_dir);
    std::fs::create_dir_all(&vault_dir).unwrap();
    let vault = vault_dir.clone();
    let sid = "019f1234-5678-7abc-8def-0123456789ab";

    let prior_socket = std::env::var("HERDR_SOCKET_PATH").ok();
    std::env::set_var("HERDR_SOCKET_PATH", vault.join("missing-herdr.sock"));
    assert!(
        crate::herdr::pane_list().await.is_none(),
        "absent herdr socket must fail soft"
    );

    let upstream_port = serve_upstream().await;
    let exports: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));

    // fake OTLP — record path/auth/body (body read requires async; do it inline)
    let otlp_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let otlp_port = otlp_listener.local_addr().unwrap().port();
    {
        let exports = exports.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = otlp_listener.accept().await else {
                    break;
                };
                let exports = exports.clone();
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let exports = exports.clone();
                        async move {
                            let path = req.uri().path().to_string();
                            let auth = req
                                .headers()
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(String::from);
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                            exports
                                .lock()
                                .unwrap()
                                .push(json!({ "path": path, "authorization": auth, "body": body }));
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"{}"))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
    }

    std::env::set_var("VAULTR_OTEL", "1");
    std::env::set_var(
        "VAULTR_OTEL_ENDPOINT",
        format!("http://127.0.0.1:{otlp_port}"),
    );
    let otel = Arc::new(crate::otel::Otel::new());
    assert!(otel.enabled);

    let ads = adapters();
    let upstream = format!("http://127.0.0.1:{upstream_port}");
    let claude_port = start_proxy(adapters().remove(0), upstream.clone(), &vault, otel.clone());
    let client = reqwest::Client::new();

    let msg = |messages: Value, tools: Value| {
        let client = client.clone();
        let url = format!("http://127.0.0.1:{claude_port}/v1/messages");
        async move {
            client
                .post(&url)
                .header("content-type", "application/json")
                .json(&json!({
                    "model": "m", "stream": true, "tools": tools, "system": "s",
                    "messages": messages,
                    "metadata": { "user_id": json!({ "session_id": sid }).to_string() },
                }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    msg(
        json!([{ "role": "user", "content": "one" }]),
        json!([{ "name": "t1" }]),
    )
    .await;
    msg(
        json!([{ "role": "user", "content": "one" }, { "role": "assistant", "content": "r" }]),
        json!([{ "name": "t1" }]),
    )
    .await;
    // compaction: rewritten shorter history must be captured losslessly
    msg(
        json!([{ "role": "user", "content": "SUMMARY" }]),
        json!([{ "name": "t1" }]),
    )
    .await;
    // capture runs async post-stream; give it a beat
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let dir = session_dir(&vault, sid).unwrap();
    assert!(
        !dir.join("herdr.jsonl").exists(),
        "absent herdr socket must not create a sidecar"
    );
    let turns: Vec<Value> = std::fs::read_to_string(dir.join("turns.jsonl"))
        .unwrap()
        .trim()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0]["schema_version"], 1);
    assert_eq!(
        turns[0]["request"]["body_delta"]["history"]["prefix_length"],
        0
    );
    assert!(
        turns[0]["request"]["body_delta"]["set"]
            .get("tools")
            .is_some(),
        "big field stored on first turn"
    );
    assert_eq!(
        turns[1]["request"]["body_delta"]["history"]["prefix_length"],
        1
    );
    assert!(
        turns[1]["request"]["body_delta"]["set"]
            .get("tools")
            .is_none(),
        "unchanged big field omitted"
    );
    assert!(
        turns[1]["request"]["body_delta"]["set"]
            .get("model")
            .is_some(),
        "small field verbatim every turn"
    );
    assert_eq!(
        turns[2]["request"]["body_delta"]["history"]["prefix_length"], 0,
        "compaction detected via LCP"
    );
    assert_eq!(
        turns[2]["request"]["body_delta"]["history"]["append"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(turns[0]["response"]["complete"], true);
    assert_eq!(turns[0]["response"]["headers"]["request-id"], "req_test");
    assert!(
        turns[0]["response"]["headers"].get("x-secret").is_none(),
        "non-allowlisted header dropped"
    );

    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["schema_version"], 1);
    assert_eq!(
        state["request_body"]["messages"],
        json!([{ "role": "user", "content": "SUMMARY" }])
    );

    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(vault.join(".meta").join(format!("{sid}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["model"], "m");
    assert_eq!(meta["harness"], "claude-code");

    // -- codex adapter (zstd + header identity) --
    let codex_port = start_proxy(
        {
            let mut v = ads;
            v.remove(1)
        },
        upstream.clone(),
        &vault,
        otel.clone(),
    );
    let csid = "019f1234-5678-7abc-8def-0123456789ac";
    let cbody = zstd::encode_all(
        json!({ "model": "gpt-test", "instructions": "be concise", "input": [{ "role": "user", "content": "hi" }] })
            .to_string()
            .as_bytes(),
        3,
    )
    .unwrap();
    let cresp = client
        .post(format!("http://127.0.0.1:{codex_port}/responses"))
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .header("session-id", csid)
        .body(cbody)
        .send()
        .await
        .unwrap();
    assert_eq!(cresp.text().await.unwrap(), SSE);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let cdir = session_dir(&vault, csid).unwrap();
    let cturns: Vec<Value> = std::fs::read_to_string(cdir.join("turns.jsonl"))
        .unwrap()
        .trim()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(cturns[0]["harness"], "codex");
    assert_eq!(cturns[0]["request"]["content_encoding"], "zstd");
    assert!(cturns[0]["request"]["body_delta"]["set"]
        .get("instructions")
        .is_some());
    assert_eq!(
        cturns[0]["request"]["body_delta"]["history"]["key"],
        "input"
    );
    assert_eq!(cturns[0]["response"]["complete"], true);

    // -- OTLP metrics + logs --
    otel.flush(&client, Some("test-token")).await;
    let exports = exports.lock().unwrap().clone();
    assert_eq!(exports.len(), 2);
    assert!(exports
        .iter()
        .all(|e| e["authorization"] == "Bearer test-token"));
    let point_attrs = |point: &Value| -> Value {
        let mut m = serde_json::Map::new();
        for a in point["attributes"].as_array().unwrap() {
            let v = &a["value"];
            let val = v
                .get("stringValue")
                .cloned()
                .or_else(|| v.get("boolValue").cloned())
                .unwrap_or_else(|| json!(v["intValue"].as_str().unwrap().parse::<i64>().unwrap()));
            m.insert(a["key"].as_str().unwrap().to_string(), val);
        }
        Value::Object(m)
    };
    let metric_export = exports.iter().find(|e| e["path"] == "/v1/metrics").unwrap();
    let metrics = metric_export["body"]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap();
    let token_points = metrics
        .iter()
        .find(|m| m["name"] == "vaultr.tokens")
        .unwrap()["sum"]["dataPoints"]
        .as_array()
        .unwrap();
    let claude_input = token_points
        .iter()
        .find(|p| {
            let a = point_attrs(p);
            a["type"] == "input" && a["harness"] == "claude-code" && a["model"] == "m"
        })
        .unwrap();
    let codex_cache = token_points
        .iter()
        .find(|p| {
            let a = point_attrs(p);
            a["type"] == "cache_read" && a["harness"] == "codex" && a["model"] == "gpt-test"
        })
        .unwrap();
    assert_eq!(claude_input["asInt"], "33");
    assert_eq!(codex_cache["asInt"], "4");
    let log_export = exports.iter().find(|e| e["path"] == "/v1/logs").unwrap();
    assert_eq!(
        log_export["body"]["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        point_attrs(&log_export["body"]["resourceLogs"][0]["resource"])["loki.resource.labels"],
        "service.namespace"
    );

    // -- 502 path --
    let broken_port = start_proxy(
        adapters().remove(0),
        "http://127.0.0.1:1".into(),
        &vault,
        otel.clone(),
    );
    let status = client
        .post(format!("http://127.0.0.1:{broken_port}/v1/messages"))
        .body("{}")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 502);

    // -- scrub: regex pattern redacted, session UUID left intact (the FP gitleaks caused) --
    {
        let sf = vault.join("scrub-test.jsonl");
        let tok = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let uuid = "019f62ae-78ad-7e80-986a-7ae4b460533c"; // must NOT be redacted
        std::fs::write(
            &sf,
            json!({ "out": format!("token={tok} sid={uuid}") }).to_string() + "\n",
        )
        .unwrap();
        assert!(sweep::scrub(&sf).await);
        let scrubbed = std::fs::read_to_string(&sf).unwrap();
        assert!(!scrubbed.contains(&tok) && scrubbed.contains("[REDACTED]"));
        assert!(
            scrubbed.contains(uuid),
            "session UUID must survive scrub (was a gitleaks false positive)"
        );
        serde_json::from_str::<Value>(scrubbed.trim()).unwrap(); // redaction must not break JSON

        // -- scrub: optional denylist redacts a plaintext string no regex can match --
        let home = vault.join("denylist-home");
        std::fs::create_dir_all(home.join(".config/wireproxy")).unwrap();
        let plain = "CorrectHorseBattery-notaregexpattern";
        std::fs::write(
            home.join(".config/wireproxy/scrub-denylist.txt"),
            format!("{plain}\n"),
        )
        .unwrap();
        let df = vault.join("scrub-denylist-test.jsonl");
        std::fs::write(
            &df,
            json!({ "out": format!("pw={plain}") }).to_string() + "\n",
        )
        .unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);
        let ok = sweep::scrub(&df).await;
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert!(ok);
        let scrubbed = std::fs::read_to_string(&df).unwrap();
        assert!(!scrubbed.contains(plain) && scrubbed.contains("[REDACTED]"));
        serde_json::from_str::<Value>(scrubbed.trim()).unwrap();
    }

    // -- herdr: socket round-trip, exact binding, sibling shape, and dedupe --
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let socket = vault.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                let mut stream = tokio::io::BufReader::new(stream);
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["method"],
                    "pane.list"
                );
                stream
                    .get_mut()
                    .write_all(
                        format!(
                            "{{\"id\":\"plant\",\"result\":{{\"panes\":[{{\"workspace_id\":\"w1\",\"tab_id\":\"w1:t1\",\"pane_id\":\"w1:p1\",\"terminal_id\":\"term1\",\"cwd\":\"/work\",\"focused\":true,\"agent\":\"claude\",\"agent_status\":\"working\",\"agent_session\":{{\"value\":\"{sid}\"}}}},{{\"workspace_id\":\"w1\",\"tab_id\":\"w1:t2\",\"pane_id\":\"w1:p2\",\"terminal_id\":\"term2\",\"cwd\":\"/work\",\"focused\":false,\"agent\":\"codex\",\"agent_status\":\"idle\",\"agent_session\":{{\"value\":\"sibling-session\"}}}}]}}}}\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        std::env::set_var("HERDR_SOCKET_PATH", &socket);
        std::env::set_var("PLANT_HERDR_INTERVAL_SECS", "0");
        crate::herdr::maybe_snapshot(&vault);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        crate::herdr::maybe_snapshot(&vault);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let snapshots: Vec<Value> = std::fs::read_to_string(dir.join("herdr.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(snapshots.len(), 1, "identical herdr snapshots must dedupe");
        assert_eq!(snapshots[0]["pane"]["pane_id"], "w1:p1");
        assert_eq!(
            snapshots[0]["siblings"][0]["agent_session_id"],
            "sibling-session"
        );
        assert!(snapshots[0]["ts"].as_str().is_some());
        std::env::remove_var("PLANT_HERDR_INTERVAL_SECS");
    }

    match prior_socket {
        Some(path) => std::env::set_var("HERDR_SOCKET_PATH", path),
        None => std::env::remove_var("HERDR_SOCKET_PATH"),
    }

    let _ = std::fs::remove_dir_all(&vault_dir);
    println!("vaultr self-test: OK");
}
