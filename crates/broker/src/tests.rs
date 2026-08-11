//! End-to-end tests over a real socket.
//!
//! The broker is bound and spoken to as a client would, because the properties
//! worth pinning here are properties of the *served surface*: which routes need
//! a tenant, what a refused caller sees, and what a malformed key is allowed to
//! reach. Testing `route()` directly would skip the one thing that has to hold —
//! that identity is established before any seal path is chosen.
//!
//! Nothing here reaches S3. Every case either stops before the store is
//! consulted, or asserts that it did.

use super::*;
use std::net::{Ipv4Addr, SocketAddrV4};

/// A broker bound to loopback with a dev tenant, pointed at a bucket that must
/// never be reached: any test that gets as far as an AWS call is a test that has
/// escaped its own scope, and the nonsense bucket name makes that loud.
async fn broker() -> (SocketAddr, tempfile::TempDir) {
    let spool = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker {
        store: Store::new("broker-tests-must-not-reach-s3"),
        resolver: Resolver::new(
            "/nonexistent/tailscaled.sock",
            Tenant::from_node_name("testhost"),
        ),
        metrics: Metrics::new(),
        keys: KeyPolicy::default(),
        spool: spool.path().to_path_buf(),
        max_object_bytes: 1024,
        listing: Mutex::new(None),
    });
    let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, broker, std::future::pending()));
    (addr, spool)
}

async fn request(
    addr: SocketAddr,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "broker.test")
        .header("content-length", body.len().to_string())
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn liveness_and_metrics_are_reachable_without_a_tenant() {
    let (addr, _spool) = broker().await;
    let (status, body) = request(addr, Method::GET, "/healthz", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    // Prometheus scrapes from inside the cluster, which is not a tailnet peer.
    // If this route ever demands a tenant, staleness alerting goes dark and the
    // failure it exists to catch becomes invisible.
    let (status, body) = request(addr, Method::GET, "/metrics", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("seal_last_contact_age_seconds"), "{body}");
}

#[tokio::test]
async fn an_unknown_route_is_a_404_after_identity_not_before() {
    let (addr, _spool) = broker().await;
    let (status, body) = request(addr, Method::GET, "/v1/anything", vec![]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("no route"), "{body}");
}

// The write grant must not be reachable by a key that is not a seal. Each of
// these stops at validation, before the store is consulted at all — which the
// unreachable bucket in `broker()` is what proves.
#[tokio::test]
async fn a_key_outside_the_seal_layout_is_refused_before_the_store_is_touched() {
    let (addr, _spool) = broker().await;
    for key in [
        "../../etc/passwd",
        "learnings/2026/08/03/abc/turns.jsonl.zst",
        "sessions/2026/08/03/abc/herdr.jsonl.zst",
        "sessions/2026/08/03/abc/.meta.json",
    ] {
        let (status, body) = request(
            addr,
            Method::PUT,
            &format!("/v1/seals/{key}"),
            b"payload".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {body}");
    }
}

#[tokio::test]
async fn an_oversized_body_is_refused_on_its_declared_length() {
    let (addr, _spool) = broker().await;
    let (status, body) = request(
        addr,
        Method::PUT,
        "/v1/seals/sessions/2026/08/03/abc/turns.jsonl.zst",
        vec![0u8; 2048], // the test broker's limit is 1 KiB
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
}

// A refused upload is a counted failure, not a silent one: the client is told,
// and the tenant's failure counter moves so a client lying about success cannot
// hide it.
#[tokio::test]
async fn a_refused_upload_leaves_the_tenant_contacted_and_uncredited() {
    let (addr, _spool) = broker().await;
    let _ = request(
        addr,
        Method::PUT,
        "/v1/seals/../../etc/passwd",
        b"x".to_vec(),
    )
    .await;
    let (_, metrics) = request(addr, Method::GET, "/metrics", vec![]).await;
    assert!(
        metrics.contains("seal_last_contact_age_seconds{tenant=\"testhost\"}"),
        "{metrics}"
    );
    assert!(
        !metrics.contains("seal_last_upload_age_seconds{tenant=\"testhost\"}"),
        "{metrics}"
    );
}

#[tokio::test]
async fn a_body_streams_to_the_spool_and_reports_its_true_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seal");
    let size = spool_body(Full::new(Bytes::from(vec![7u8; 4096])), &path, 8192)
        .await
        .unwrap();
    assert_eq!(size, 4096);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);

    // The limit is a refusal, never a truncation: a seal cut short and stored
    // would be a corrupt copy presented as a durable one.
    assert!(
        spool_body(Full::new(Bytes::from(vec![7u8; 4096])), &path, 100)
            .await
            .is_err()
    );
}
