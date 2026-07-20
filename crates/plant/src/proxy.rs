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
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// ponytail: gate of 2 bounds peak RSS of concurrent JSON-DOM parses; raise if capture latency ever matters
static DOM_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[cfg(test)]
static ACTIVE_CAPTURE_TASKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SKIP_CAPTURE_FINISH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct CaptureTaskGuard;

impl CaptureTaskGuard {
    fn new() -> Self {
        #[cfg(test)]
        ACTIVE_CAPTURE_TASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for CaptureTaskGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        ACTIVE_CAPTURE_TASKS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct CaptureTasks {
    tracker: TaskTracker,
    abort: CancellationToken,
}

impl CaptureTasks {
    fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            abort: CancellationToken::new(),
        }
    }

    fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let abort = self.abort.clone();
        drop(self.tracker.spawn(async move {
            let _guard = CaptureTaskGuard::new();
            tokio::select! {
                biased;
                () = abort.cancelled() => {}
                () = task => {}
            }
        }));
    }

    fn close(&self) {
        self.tracker.close();
    }

    fn abort(&self) {
        self.abort.cancel();
    }

    async fn wait(&self) {
        self.tracker.wait().await;
    }
}

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
    let _lease = serve_with_shutdown(
        listener,
        ctx,
        std::future::pending(),
        Duration::from_secs(30),
    )
    .await;
}

pub async fn serve_until_shutdown(
    listener: TcpListener,
    ctx: Arc<ProxyCtx>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    drain_timeout: Duration,
) -> TcpListener {
    serve_with_shutdown(
        listener,
        ctx,
        async move {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        },
        drain_timeout,
    )
    .await
}

async fn serve_with_shutdown(
    listener: TcpListener,
    ctx: Arc<ProxyCtx>,
    shutdown: impl std::future::Future<Output = ()>,
    drain_timeout: Duration,
) -> TcpListener {
    tokio::pin!(shutdown);
    let mut connections = JoinSet::new();
    let capture_tasks = CaptureTasks::new();
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut shutdown => break,
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    eprintln!("connection task failed: {error}");
                }
                continue;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, _) = match accepted {
            Ok(x) => x,
            Err(e) => {
                eprintln!(
                    "[{}] accept error: {e}",
                    ctx.adapter.harness.capture_label()
                );
                continue;
            }
        };
        let ctx = ctx.clone();
        let capture_tasks = capture_tasks.clone();
        connections.spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |req| {
                let ctx = ctx.clone();
                let capture_tasks = capture_tasks.clone();
                async move { Ok::<_, Infallible>(handle(req, ctx, capture_tasks).await) }
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

    // Cancellation fixes the in-flight set: no new accept branch is polled while
    // these exact connection and capture tasks drain, and the listener stays owned.
    capture_tasks.close();
    let deadline = tokio::time::Instant::now() + drain_timeout;
    let mut connections_timed_out = false;
    while !connections.is_empty() {
        match tokio::time::timeout_at(deadline, connections.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => eprintln!("connection task failed: {error}"),
            Ok(None) => break,
            Err(_) => {
                connections_timed_out = true;
                break;
            }
        }
    }
    if connections_timed_out {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    if connections_timed_out
        || tokio::time::timeout_at(deadline, capture_tasks.wait())
            .await
            .is_err()
    {
        capture_tasks.abort();
    }
    capture_tasks.wait().await;
    listener
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<ProxyCtx>,
    capture_tasks: CaptureTasks,
) -> Response<BoxBody> {
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
        let body = health_body(adapter);
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
            eprintln!(
                "[{}] upstream fetch failed: {e}",
                adapter.harness.capture_label()
            );
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
                    body_sha256: vaultr::vault::sha256_hex(&decoded),
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
                eprintln!("[{}] capture failed: {e}", adapter.harness.capture_label());
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
    capture_tasks.spawn(async move {
        let mut chunks: Vec<u8> = Vec::new();
        let mut complete = true;
        loop {
            let next = tokio::select! {
                biased;
                () = tx.closed() => {
                    complete = false;
                    break;
                }
                next = upstream_stream.next() => next,
            };
            match next {
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
        #[cfg(test)]
        if SKIP_CAPTURE_FINISH.load(std::sync::atomic::Ordering::SeqCst) {
            capture::release_memory();
            return;
        }
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

pub(crate) fn health_body(adapter: &Adapter) -> serde_json::Value {
    serde_json::json!({
        "service": "plant",
        "ok": true,
        "harness": adapter.harness.capture_label(),
        "upstream": adapter.upstream.trim_end_matches('/'),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct SkipCaptureFinish;

    impl SkipCaptureFinish {
        fn new() -> Self {
            SKIP_CAPTURE_FINISH.store(true, std::sync::atomic::Ordering::SeqCst);
            Self
        }
    }

    impl Drop for SkipCaptureFinish {
        fn drop(&mut self) {
            SKIP_CAPTURE_FINISH.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn blocking_upstream() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let started_tx = started_tx.clone();
                let mut release_rx = release_rx.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request).await;
                    let _ = started_tx.send(());
                    while !*release_rx.borrow() {
                        if release_rx.changed().await.is_err() {
                            return;
                        }
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        (address, started_rx, release_tx, task)
    }

    async fn stalled_stream_upstream() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        (address, task)
    }

    fn test_ctx(upstream: std::net::SocketAddr) -> Arc<ProxyCtx> {
        let mut adapter = crate::adapter::adapters().remove(0);
        adapter.upstream = format!("http://{upstream}");
        Arc::new(ProxyCtx {
            adapter,
            vault: std::env::temp_dir(),
            client: reqwest::Client::new(),
            otel: Arc::new(Otel::new()),
        })
    }

    #[tokio::test]
    async fn shutdown_stops_accepting_but_finishes_the_fixed_connection_set() {
        let (upstream, mut started, release, upstream_task) = blocking_upstream().await;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let supervisor = tokio::spawn(serve_until_shutdown(
            listener,
            test_ctx(upstream),
            shutdown_rx,
            Duration::from_secs(2),
        ));

        let first = tokio::spawn(async move {
            reqwest::get(format!("http://{address}/block"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(1), started.recv())
            .await
            .expect("first connection was not accepted")
            .expect("upstream stopped");

        shutdown_tx.send(true).unwrap();
        let second =
            tokio::spawn(
                async move { reqwest::get(format!("http://{address}/after-cancel")).await },
            );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), started.recv())
                .await
                .is_err(),
            "a post-cancellation connection reached the upstream"
        );
        assert!(
            TcpListener::bind(address).await.is_err(),
            "listener lease was released while the fixed set was draining"
        );

        release.send(true).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), first)
                .await
                .unwrap()
                .unwrap(),
            "ok"
        );
        let lease = tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .unwrap()
            .unwrap();
        assert!(TcpListener::bind(address).await.is_err());
        drop(lease);
        second.abort();
        let _ = second.await;
        drop(TcpListener::bind(address).await.unwrap());
        upstream_task.abort();
        let _ = upstream_task.await;
    }

    #[tokio::test]
    async fn shutdown_aborts_and_reaps_connections_at_the_drain_deadline() {
        let (upstream, mut started, _release, upstream_task) = blocking_upstream().await;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let supervisor = tokio::spawn(serve_until_shutdown(
            listener,
            test_ctx(upstream),
            shutdown_rx,
            Duration::from_millis(50),
        ));
        let request =
            tokio::spawn(async move { reqwest::get(format!("http://{address}/never")).await });
        tokio::time::timeout(Duration::from_secs(1), started.recv())
            .await
            .expect("connection was not accepted")
            .expect("upstream stopped");

        shutdown_tx.send(true).unwrap();
        let lease = tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .expect("connection drain exceeded its deadline")
            .unwrap();
        assert!(
            TcpListener::bind(address).await.is_err(),
            "listener lease was released before aborted tasks were reaped"
        );
        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .expect("aborted client connection stayed open")
            .unwrap()
            .expect_err("aborted connection unexpectedly completed");
        drop(lease);
        drop(TcpListener::bind(address).await.unwrap());

        upstream_task.abort();
        let _ = upstream_task.await;
    }

    #[tokio::test]
    async fn captured_stalled_stream_finalizer_ends_before_listener_lease_return() {
        let _skip_finish = SkipCaptureFinish::new();
        let (upstream, upstream_task) = stalled_stream_upstream().await;
        let vault = std::env::temp_dir().join(format!("plant-proxy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&vault).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut ctx = test_ctx(upstream);
        Arc::get_mut(&mut ctx).unwrap().vault.clone_from(&vault);
        let supervisor = tokio::spawn(serve_until_shutdown(
            listener,
            ctx,
            shutdown_rx,
            Duration::from_secs(5),
        ));

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let body = serde_json::json!({
            "metadata": {
                "user_id": "{\"session_id\":\"00000000-0000-4000-8000-000000000001\"}"
            },
            "messages": [],
            "model": "test"
        })
        .to_string();
        client
            .write_all(
                format!(
                    "POST /v1/messages HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = [0_u8; 512];
        let bytes = tokio::time::timeout(Duration::from_secs(1), client.read(&mut response))
            .await
            .expect("captured response headers stalled")
            .unwrap();
        assert!(String::from_utf8_lossy(&response[..bytes]).contains("200 OK"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while ACTIVE_CAPTURE_TASKS.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("capture finalizer never started");

        drop(client);
        shutdown_tx.send(true).unwrap();
        let lease = tokio::time::timeout(Duration::from_secs(2), supervisor)
            .await
            .expect("stalled capture outlived client disconnect")
            .unwrap();
        assert_eq!(
            ACTIVE_CAPTURE_TASKS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "capture finalizer survived listener lease return"
        );
        assert!(TcpListener::bind(address).await.is_err());
        drop(lease);
        drop(TcpListener::bind(address).await.unwrap());

        upstream_task.abort();
        let _ = upstream_task.await;
        std::fs::remove_dir_all(vault).unwrap();
    }
}
