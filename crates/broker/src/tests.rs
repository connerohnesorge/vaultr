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
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A broker bound to loopback with a dev tenant, pointed at a bucket that must
/// never be reached: any test that gets as far as an AWS call is a test that has
/// escaped its own scope, and the nonsense bucket name makes that loud.
async fn broker_on(surface: Surface) -> (SocketAddr, tempfile::TempDir) {
    broker_with(
        surface,
        Store::with_max_aws_processes(
            "broker-tests-must-not-reach-s3",
            store::DEFAULT_MAX_AWS_PROCESSES,
        ),
        Resolver::new(
            Some("/nonexistent/tailscaled.sock".into()),
            Tenant::from_node_name("testhost"),
        ),
        HashSet::new(),
    )
    .await
}

async fn broker_with(
    surface: Surface,
    store: Store,
    resolver: Resolver,
    read_tenants: HashSet<Tenant>,
) -> (SocketAddr, tempfile::TempDir) {
    let spool = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker {
        store,
        resolver,
        metrics: Metrics::new(),
        keys: KeyPolicy::default(),
        read_tenants,
        spool: spool.path().to_path_buf(),
        max_object_bytes: 1024,
        listing: Mutex::new(None),
    });
    let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        broker,
        std::future::pending(),
        surface,
        DEFAULT_DRAIN_TIMEOUT,
    ));
    (addr, spool)
}

async fn broker() -> (SocketAddr, tempfile::TempDir) {
    broker_on(Surface::TenantApi).await
}

#[tokio::test]
async fn shutdown_closes_an_idle_keepalive_connection_without_waiting_for_kubelet_kill() {
    let spool = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker {
        store: Store::with_max_aws_processes(
            "broker-tests-must-not-reach-s3",
            store::DEFAULT_MAX_AWS_PROCESSES,
        ),
        resolver: Resolver::new(
            Some("/nonexistent/tailscaled.sock".into()),
            Tenant::from_node_name("testhost"),
        ),
        metrics: Metrics::new(),
        keys: KeyPolicy::default(),
        read_tenants: HashSet::new(),
        spool: spool.path().to_path_buf(),
        max_object_bytes: 1024,
        listing: Mutex::new(None),
    });
    let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        broker,
        async {
            let _ = shutdown_rx.await;
        },
        Surface::TenantApi,
        Duration::from_secs(1),
    ));

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: broker.test\r\n\r\n")
        .await
        .unwrap();
    let mut response = [0u8; 256];
    let bytes = client.read(&mut response).await.unwrap();
    assert!(String::from_utf8_lossy(&response[..bytes]).contains("200 OK"));

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("idle keep-alive must not hold the server open")
        .unwrap();
}

#[tokio::test]
async fn shutdown_aborts_a_stuck_body_after_the_bounded_drain_window() {
    let spool = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker {
        store: Store::with_max_aws_processes(
            "broker-tests-must-not-reach-s3",
            store::DEFAULT_MAX_AWS_PROCESSES,
        ),
        resolver: Resolver::new(
            Some("/nonexistent/tailscaled.sock".into()),
            Tenant::from_node_name("testhost"),
        ),
        metrics: Metrics::new(),
        keys: KeyPolicy::default(),
        read_tenants: HashSet::new(),
        spool: spool.path().to_path_buf(),
        max_object_bytes: 1024,
        listing: Mutex::new(None),
    });
    let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        broker,
        async {
            let _ = shutdown_rx.await;
        },
        Surface::TenantApi,
        Duration::from_millis(50),
    ));

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"PUT /v1/seals/sessions/2026/08/17/drain-test/turns.jsonl.zst \
              HTTP/1.1\r\nHost: broker.test\r\nContent-Length: 100\r\n\r\nx",
        )
        .await
        .unwrap();
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("stuck request must be bounded by the drain timeout")
        .unwrap();
}

async fn request_response(
    addr: SocketAddr,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> hyper::Response<Incoming> {
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
    sender.send_request(request).await.unwrap()
}

async fn request(
    addr: SocketAddr,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let response = request_response(addr, method, path, body).await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).to_string())
}

#[test]
fn read_tenant_configuration_is_normalized_and_deduplicated() {
    let tenants = configured_read_tenants(Some("CB14957,cb14957.hs.cnb.rocks".into())).unwrap();
    assert_eq!(tenants.len(), 1);
    assert!(tenants.contains(&Tenant::from_node_name("cb14957").unwrap()));
    assert!(configured_read_tenants(Some("CB14957,,other".into())).is_err());
}

#[tokio::test]
async fn an_authorized_read_returns_a_short_lived_presigned_redirect() {
    let dir = tempfile::tempdir().unwrap();
    let aws = dir.path().join("aws");
    std::fs::write(
        &aws,
        "#!/bin/sh\n[ \"$1\" = s3 ] && [ \"$2\" = presign ] || exit 1\n[ \"$4\" = --expires-in ] && [ \"$5\" = 300 ] || exit 1\nprintf '%s\\n' 'https://signed.example.test/seal?X-Amz-Signature=fixture'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&aws).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&aws, permissions).unwrap();

    let tenant = Tenant::from_node_name("CB14957").unwrap();
    let mut read_tenants = HashSet::new();
    read_tenants.insert(tenant.clone());
    let (addr, _spool) = broker_with(
        Surface::TenantApi,
        Store::with_aws_binary("broker-tests", store::DEFAULT_MAX_AWS_PROCESSES, aws),
        Resolver::new(None, Some(tenant)),
        read_tenants,
    )
    .await;

    let response = request_response(
        addr,
        Method::GET,
        "/v1/seals/sessions/2026/08/03/abc/turns.jsonl.zst",
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response
            .headers()
            .get(hyper::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "https://signed.example.test/seal?X-Amz-Signature=fixture"
    );
    assert_eq!(
        response
            .headers()
            .get(hyper::header::CACHE_CONTROL)
            .unwrap(),
        "no-store"
    );
}

#[tokio::test]
async fn an_unauthorized_tenant_cannot_read_a_seal() {
    let mut read_tenants = HashSet::new();
    read_tenants.insert(Tenant::from_node_name("CB14957").unwrap());
    let (addr, _spool) = broker_with(
        Surface::TenantApi,
        Store::with_max_aws_processes(
            "broker-tests-must-not-reach-s3",
            store::DEFAULT_MAX_AWS_PROCESSES,
        ),
        Resolver::new(None, Some(Tenant::from_node_name("other-tenant").unwrap())),
        read_tenants,
    )
    .await;

    let (status, body) = request(
        addr,
        Method::GET,
        "/v1/seals/sessions/2026/08/03/abc/turns.jsonl.zst",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("not authorized"), "{body}");
}

#[tokio::test]
async fn an_existing_tenant_can_still_upload_a_seal() {
    let dir = tempfile::tempdir().unwrap();
    let aws = dir.path().join("aws");
    std::fs::write(
        &aws,
        "#!/bin/sh\ncase \"$1:$2\" in\ns3api:list-objects-v2) printf '%s\\n' None ;;\ns3api:put-object) printf '%s\\n' '{}' ;;\n*) exit 1 ;;\nesac\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&aws).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&aws, permissions).unwrap();

    let (addr, _spool) = broker_with(
        Surface::TenantApi,
        Store::with_aws_binary("broker-tests", store::DEFAULT_MAX_AWS_PROCESSES, aws),
        Resolver::new(None, Some(Tenant::from_node_name("other-tenant").unwrap())),
        HashSet::new(),
    )
    .await;
    let (status, body) = request(
        addr,
        Method::PUT,
        "/v1/seals/sessions/2026/08/03/abc/turns.jsonl.zst",
        b"payload".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("uploaded"), "{body}");
}

#[tokio::test]
async fn an_authorized_read_rejects_a_non_seal_key_before_the_store() {
    let mut read_tenants = HashSet::new();
    let tenant = Tenant::from_node_name("CB14957").unwrap();
    read_tenants.insert(tenant.clone());
    let (addr, _spool) = broker_with(
        Surface::TenantApi,
        Store::with_max_aws_processes(
            "broker-tests-must-not-reach-s3",
            store::DEFAULT_MAX_AWS_PROCESSES,
        ),
        Resolver::new(None, Some(tenant)),
        read_tenants,
    )
    .await;

    let (status, body) = request(addr, Method::GET, "/v1/seals/not-a-seal", vec![]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("seal key"), "{body}");
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
async fn observability_listener_cannot_reach_any_tenant_route() {
    let (addr, _spool) = broker_on(Surface::Observability).await;
    let (status, body) = request(addr, Method::GET, "/healthz", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let (status, body) = request(addr, Method::GET, "/metrics", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("seal_last_contact_age_seconds"), "{body}");

    let (status, body) = request(addr, Method::GET, "/v1/seals", vec![]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("serves only"), "{body}");
}

#[tokio::test]
async fn stale_uploads_are_removed_but_unrelated_spool_files_survive() {
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("seal-deadbeef.upload");
    let keep = dir.path().join("keep.upload");
    tokio::fs::write(&stale, b"partial").await.unwrap();
    tokio::fs::write(&keep, b"unrelated").await.unwrap();

    assert_eq!(clean_stale_spool(dir.path()).await.unwrap(), 1);
    assert!(!stale.exists());
    assert!(keep.exists());
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
