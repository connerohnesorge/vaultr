//! Reverse proxy — hyper server, reqwest streaming upstream, capture tee.
//! Ports startProxy/capturedStream (wireproxy.ts:437-525).

use crate::adapter::Adapter;
use crate::capture::{self, CapturedRequest, CapturedResponse};
use crate::otel::Otel;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::{Request, Response, StatusCode};
use serde_json::Value;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::net::TcpListener;

pub type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// ponytail: gate of 2 bounds peak RSS of concurrent JSON-DOM parses; raise if capture latency ever matters
static DOM_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

fn full(data: impl Into<Bytes>) -> BoxBody {
    Full::new(data.into())
        .map_err(|e: Infallible| match e {})
        .boxed()
}

fn json_error(status: StatusCode, message: &str) -> Response<BoxBody> {
    let body = serde_json::json!({ "type": "error", "error": { "type": "api_error", "message": message } });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(body.to_string()))
        .unwrap()
}

pub struct ProxyCtx {
    pub adapter: Adapter,
    pub vault: PathBuf,
    pub client: reqwest::Client,
    pub otel: Arc<Otel>,
}

/// Bind the listener. Separate from serve() so main can detect EADDRINUSE and exit 0.
pub async fn bind(port: u16) -> std::io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

pub async fn serve(listener: TcpListener, ctx: Arc<ProxyCtx>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[{}] accept error: {e}", ctx.adapter.harness);
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |req| {
                let ctx = ctx.clone();
                async move { Ok::<_, Infallible>(handle(req, ctx).await) }
            });
            // no idle timeout: long thinking pauses are normal
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                let msg = e.to_string();
                // client disconnects are routine noise
                if !msg.contains("connection closed") {
                    eprintln!("serve error: {msg}");
                }
            }
        });
    }
}

async fn handle(req: Request<hyper::body::Incoming>, ctx: Arc<ProxyCtx>) -> Response<BoxBody> {
    let started_at = SystemTime::now();
    let adapter = &ctx.adapter;
    let upstream_base = adapter.upstream.trim_end_matches('/');
    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();

    if path == "/health" {
        let body = serde_json::json!({ "ok": true, "harness": adapter.harness, "upstream": upstream_base });
        return Response::builder()
            .header("content-type", "application/json")
            .body(full(body.to_string()))
            .unwrap();
    }
    if req
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
    {
        return Response::builder()
            .status(426)
            .body(full("HTTP fallback required\n"))
            .unwrap();
    }

    let method = req.method().as_str().to_string();
    let has_body = method != "GET" && method != "HEAD";
    let req_headers = req.headers().clone();
    let raw: Option<Bytes> = if has_body {
        match req.into_body().collect().await {
            Ok(c) => Some(c.to_bytes()),
            Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("vaultr: {e}")),
        }
    } else {
        None
    };

    // forward headers: keep content-encoding (body forwarded as-is), drop hop-by-hop.
    // accept-encoding is forced to identity: a zstd-compressed SSE response makes the
    // decoder allocate a 128MB window PER STREAM (the historic multi-GB RSS), and we
    // strip the encoding before the client anyway — compression buys nothing here.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "accept-encoding",
        reqwest::header::HeaderValue::from_static("identity"),
    );
    for (k, v) in &req_headers {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host" | "content-length" | "connection" | "accept-encoding"
        ) {
            continue;
        }
        if let (Ok(n), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            headers.append(n, val);
        }
    }

    let url = format!("{upstream_base}{path}{query}");
    let mut builder = ctx
        .client
        .request(method.parse().unwrap_or(reqwest::Method::POST), &url)
        .headers(headers);
    if let Some(ref raw) = raw {
        builder = builder.body(raw.clone());
    }
    let upstream = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[{}] upstream fetch failed: {e}", adapter.harness);
            return json_error(StatusCode::BAD_GATEWAY, &format!("vaultr upstream: {e}"));
        }
    };

    let status = upstream.status().as_u16();
    let mut response_headers = hyper::HeaderMap::new();
    for (k, v) in upstream.headers() {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "content-length" | "content-encoding" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        if let (Ok(n), Ok(val)) = (
            hyper::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            hyper::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            response_headers.append(n, val);
        }
    }
    let upstream_headers_full = upstream.headers().clone();

    // Request-time capture half, under a semaphore: parsing a multi-MB body into
    // a JSON DOM costs ~10x its size; bounding concurrency bounds peak RSS.
    let capturing = adapter.captures(&method, &path) && raw.is_some();
    let pending: Option<capture::PendingCapture> = if capturing {
        let _permit = DOM_GATE.acquire().await;
        let raw = raw.as_ref().unwrap();
        let content_encoding = req_headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let prepared = decode_bytes(raw, content_encoding.as_deref())
            .map_err(|e| e.to_string())
            .and_then(|decoded| {
                let body: Value = serde_json::from_slice(&decoded).map_err(|e| e.to_string())?;
                let ids = adapter.identity(&req_headers, &body);
                let req_info = CapturedRequest {
                    method,
                    path,
                    content_encoding,
                    body_sha256: capture::sha256_hex(&decoded),
                    ids,
                    started_at,
                };
                drop(decoded);
                Ok((req_info, body))
            });
        let prepared = match prepared {
            Ok((req_info, body)) => {
                capture::prepare_capture(&ctx.vault, adapter, req_info, body).await
            }
            Err(e) => Err(e),
        };
        capture::release_memory();
        match prepared {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("[{}] capture failed: {e}", adapter.harness);
                None
            }
        }
    } else {
        None
    };

    let mut resp_builder = Response::builder().status(status);
    *resp_builder.headers_mut().unwrap() = response_headers;

    let Some(pending) = pending else {
        // passthrough: stream upstream body without capture
        let stream = upstream.bytes_stream().map(|r| {
            r.map(Frame::data)
                .map_err(|e| std::io::Error::other(e.to_string()))
        });
        return resp_builder
            .body(BodyExt::boxed(StreamBody::new(stream)))
            .unwrap();
    };

    // tee: forward chunks live, save accumulated SSE at close/error
    let ctx2 = ctx.clone();
    let mut upstream_stream = upstream.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut chunks: Vec<u8> = Vec::new();
        let mut complete = true;
        loop {
            match upstream_stream.next().await {
                Some(Ok(chunk)) => {
                    chunks.extend_from_slice(&chunk);
                    // client gone (rx dropped) => stop pulling, mark incomplete
                    if tx.send(Ok(Frame::data(chunk))).await.is_err() {
                        complete = false;
                        break;
                    }
                }
                Some(Err(e)) => {
                    // torn stream: keep the partial, mark incomplete
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    complete = false;
                    break;
                }
                None => break,
            }
        }
        let sse = String::from_utf8_lossy(&chunks).into_owned();
        drop(chunks);
        let resp = CapturedResponse {
            status,
            headers: upstream_headers_full,
            complete: ctx2.adapter.response_complete(&sse, complete),
            sse,
        };
        ctx2.otel
            .record(&ctx2.adapter, pending.model.as_deref(), &pending.req, &resp);
        if let Err(e) = capture::finish_capture(&ctx2.vault, &ctx2.adapter, pending, &resp).await {
            eprintln!("capture failed: {e}");
        }
        capture::release_memory();
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    resp_builder
        .body(BodyExt::boxed(StreamBody::new(stream)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::adapters;
    use serde_json::json;

    const CLAUDE_TEXT_ONLY: &str = concat!(
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",",
        "\"text\":\"message_stop\"}}\n\n"
    );
    const CLAUDE_TERMINAL: &str = "data: {\"type\":\"message_stop\"}\n\n";
    const CODEX_TEXT_ONLY: &str = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",",
        "\"text\":\"response.completed\"}]}}\n\n"
    );

    #[derive(Clone, Copy, PartialEq)]
    enum UpstreamResponse {
        Full(&'static str),
        Torn(&'static str),
        Delayed(&'static str),
    }

    fn response_body(response: UpstreamResponse) -> BoxBody {
        match response {
            UpstreamResponse::Full(sse) => full(sse),
            UpstreamResponse::Torn(sse) => {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(2);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(Frame::data(Bytes::from_static(sse.as_bytes()))))
                        .await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let _ = tx.send(Err(std::io::Error::other("torn upstream"))).await;
                });
                BodyExt::boxed(StreamBody::new(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
            }
            UpstreamResponse::Delayed(sse) => {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(2);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(Frame::data(Bytes::from_static(sse.as_bytes()))))
                        .await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let _ = tx.send(Ok(Frame::data(Bytes::from_static(b"\n")))).await;
                });
                BodyExt::boxed(StreamBody::new(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
            }
        }
    }

    async fn start_upstream(response: UpstreamResponse) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let service = hyper::service::service_fn(
                        move |req: Request<hyper::body::Incoming>| async move {
                            let _ = req.into_body().collect().await;
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .header("request-id", "req_test")
                                    .body(response_body(response))
                                    .unwrap(),
                            )
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    async fn wait_for_capture(vault: &PathBuf, sid: &str) -> (Value, PathBuf) {
        for _ in 0..300 {
            if let Ok(session) = vaultr::vault::resolve_id(vault, sid) {
                let dir = vaultr::vault::session_dir(vault, &session).unwrap();
                let path = dir.join("turns.jsonl");
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Some(line) = text.lines().next() {
                        return (serde_json::from_str(line).unwrap(), path);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("capture did not finish for {sid}");
    }

    async fn exercise(
        label: &str,
        response: UpstreamResponse,
        codex: bool,
        disconnect: bool,
    ) -> (Value, PathBuf, Arc<Otel>, PathBuf) {
        let vault =
            std::env::temp_dir().join(format!("plant-proxy-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&vault);
        std::fs::create_dir_all(&vault).unwrap();
        let sid = uuid::Uuid::new_v4().to_string();
        let upstream = start_upstream(response).await;
        let mut adapter = adapters().remove(usize::from(codex));
        adapter.upstream = upstream;
        let otel = Arc::new(Otel::enabled_for_test());
        let (listener, port) = bind(0).await.unwrap();
        let ctx = Arc::new(ProxyCtx {
            adapter,
            vault: vault.clone(),
            client: crate::http_client(),
            otel: otel.clone(),
        });
        tokio::spawn(serve(listener, ctx));

        let client = reqwest::Client::new();
        let request = if codex {
            client
                .post(format!("http://127.0.0.1:{port}/responses"))
                .header("session-id", &sid)
                .json(&json!({
                    "model": "gpt-test",
                    "input": [{"role": "user", "content": "hi"}],
                }))
        } else {
            client
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .json(&json!({
                    "model": "claude-test",
                    "stream": true,
                    "messages": [{"role": "user", "content": "hi"}],
                    "metadata": {"user_id": json!({"session_id": &sid}).to_string()},
                }))
        };
        let response_body = request.send().await.unwrap();
        if disconnect {
            drop(response_body);
        } else {
            let body = response_body.bytes().await;
            if response == UpstreamResponse::Torn(CLAUDE_TERMINAL) {
                assert!(
                    body.is_err(),
                    "torn upstream reaches the client as an error"
                );
            } else {
                body.unwrap();
            }
        }
        let (envelope, capture_path) = wait_for_capture(&vault, &sid).await;
        (envelope, capture_path, otel, vault)
    }

    #[tokio::test]
    async fn completion_certification_controls_capture_telemetry_and_trailing_output() {
        let cases = [
            (
                "claude-text",
                UpstreamResponse::Full(CLAUDE_TEXT_ONLY),
                false,
                false,
                false,
            ),
            (
                "claude-exact",
                UpstreamResponse::Full(CLAUDE_TERMINAL),
                false,
                false,
                true,
            ),
            (
                "claude-torn",
                UpstreamResponse::Torn(CLAUDE_TERMINAL),
                false,
                false,
                false,
            ),
            (
                "claude-disconnect",
                UpstreamResponse::Delayed(CLAUDE_TERMINAL),
                false,
                true,
                false,
            ),
            (
                "codex-text",
                UpstreamResponse::Full(CODEX_TEXT_ONLY),
                true,
                false,
                false,
            ),
        ];

        for (label, response, codex, disconnect, expected) in cases {
            let (envelope, capture_path, otel, vault) =
                exercise(label, response, codex, disconnect).await;
            assert_eq!(envelope["response"]["complete"], expected, "{label}");
            assert_eq!(otel.recorded_completeness(), [expected], "{label}");
            if codex {
                let reconstructed = vaultr::recon::reconstruct(&capture_path).unwrap();
                assert_eq!(
                    reconstructed.trailing_appended, 0,
                    "uncertified Codex output_item.done must not become trailing output"
                );
            }
            let _ = std::fs::remove_dir_all(vault);
        }
    }
}

pub fn decode_bytes(raw: &[u8], encoding: Option<&str>) -> std::io::Result<Vec<u8>> {
    let is_zstd = encoding
        .map(|e| e.split(',').any(|x| x.trim() == "zstd"))
        .unwrap_or(false);
    if is_zstd {
        zstd::decode_all(raw)
    } else {
        Ok(raw.to_vec())
    }
}
