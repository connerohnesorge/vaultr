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
        let events = vaultr::recon::parse_sse(&sse);
        let resp = CapturedResponse {
            status,
            headers: upstream_headers_full,
            complete: ctx2.adapter.response_complete(&events, complete),
            sse,
        };
        ctx2.otel.record(
            &ctx2.adapter,
            pending.model.as_deref(),
            &pending.req,
            &resp,
            &events,
        );
        drop(events);
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
