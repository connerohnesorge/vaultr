//! plant-broker — the one holder of credentials reaching the seal store.
//!
//! Seals left git on 2026-08-11, and with them the accidental offsite copy a
//! `git push` had been providing every 30 minutes. This service is what replaces
//! it: plant instances — the Mac today, every vended computer later — hand it
//! bytes over the tailnet, and it is the only thing anywhere holding an IAM
//! grant that reaches `pantheon-vault-seals-athens`.
//!
//! ## Why a broker rather than a credential on each plant
//!
//! The obvious design is smaller: mint short-lived scoped S3 credentials and let
//! plant upload directly, keeping the data path unchanged and adding no single
//! point of failure. For one Mac that is the better design, and it was on the
//! table. It dies on tenancy. The seal bucket is deliberately on the deny side of
//! the agent-sandbox allow list — agent-transcript data is classified regulated —
//! and plant runs inside *every* vended computer, i.e. inside every agent
//! sandbox. "Plant holds the write credential" therefore means every sandbox's
//! machine identity gains reach into the store holding every session transcript.
//! A credential vendor has the same defect with a shorter TTL.
//!
//! A broker is the only shape where a sandbox can contribute a seal without its
//! own identity ever holding seal-bucket access. That property is unavailable to
//! any direct-to-S3 design at any credential setting, and it is the whole
//! justification for the extra hop.
//!
//! **The cost is accepted on the record:** a broker in the write path is a new
//! single point of failure for durability. If it is down, no tenant achieves an
//! offsite copy. That is why the staleness export in `metrics` is load-bearing
//! rather than nice-to-have, and why the client must treat broker-unreachable as
//! a loud failure and never as a skip.
//!
//! ## Shape general, surface narrow
//!
//! This is the general plant broker with seal upload as its only implemented
//! route. The shape is justified by tenancy, not by speculation about future
//! routes; widening IAM later is one line, and getting the service shape wrong
//! is a rewrite. The grant stays write-only (`ListBucket` + `PutObject`, no
//! `GetObject`) until a read route genuinely exists, so the worst a compromised
//! broker can do to 7.46 GB of transcripts is write garbage over versioned
//! objects.
//!
//! ## Routes
//!
//! | Route | Auth | Purpose |
//! |---|---|---|
//! | `GET /healthz` | open | liveness |
//! | `GET /metrics` | open | Prometheus scrape (counts and ages, no seal content) |
//! | `GET /v1/seals` | tenant | the store's view — `<key>\t<size>` — for reconciling |
//! | `PUT /v1/seals/<key>` | tenant | store one seal, idempotently |
//!
//! `/healthz` and `/metrics` are deliberately open: the scrape comes from
//! Prometheus inside the cluster, which is not a tailnet peer, and neither route
//! discloses anything but counts and ages.

mod metrics;
mod seal;
mod store;
mod tenant;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use metrics::{Metrics, Outcome};
use seal::KeyPolicy;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use store::{Object, Store};
use tenant::{Resolver, Tenant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

struct Broker {
    store: Store,
    resolver: Resolver,
    metrics: Metrics,
    keys: KeyPolicy,
    spool: PathBuf,
    max_object_bytes: u64,
    listing: Mutex<Option<(Vec<Object>, Instant)>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let listen: SocketAddr = env("SEAL_BROKER_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8080".into())
        .parse()
        .context("SEAL_BROKER_LISTEN must be <addr>:<port>")?;
    let spool = env("SEAL_BROKER_SPOOL")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    tokio::fs::create_dir_all(&spool)
        .await
        .with_context(|| format!("create the seal spool directory {}", spool.display()))?;
    let dev_tenant = env("SEAL_BROKER_DEV_TENANT").and_then(|name| Tenant::from_node_name(&name));
    let seal_files: Vec<String> = env("SEAL_BROKER_SEAL_FILES")
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_else(|| {
            seal::DEFAULT_SEAL_FILES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

    let broker = Arc::new(Broker {
        store: Store::new(
            env("SEAL_BROKER_BUCKET").unwrap_or_else(|| store::DEFAULT_BUCKET.into()),
        ),
        resolver: Resolver::new(
            env("SEAL_BROKER_TAILSCALE_SOCKET").unwrap_or_else(|| tenant::DEFAULT_SOCKET.into()),
            dev_tenant,
        ),
        metrics: Metrics::new(),
        keys: KeyPolicy::new(seal_files),
        spool,
        max_object_bytes: env("SEAL_BROKER_MAX_OBJECT_BYTES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(store::MAX_OBJECT_BYTES),
        listing: Mutex::new(None),
    });

    eprintln!(
        "[broker] store s3://{} seals {} spool {}",
        broker.store.bucket(),
        broker.keys.seal_files().join(","),
        broker.spool.display()
    );
    // Loud, every start, because it is the one setting that lets a caller be a
    // tenant without proving anything. It covers loopback only, and it must never
    // be quietly true in athens.
    if let Some(dev) = broker.resolver.dev_tenant() {
        eprintln!(
            "[broker] WARNING: SEAL_BROKER_DEV_TENANT={dev} — loopback callers are \
             accepted as this tenant without tailnet identity. Local proving only."
        );
    }

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    eprintln!("[broker] listening on {}", listener.local_addr()?);
    serve(listener, broker, shutdown()).await;
    Ok(())
}

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            eprintln!("[broker] cannot watch SIGTERM: {error}");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    eprintln!("[broker] draining");
}

async fn serve(
    listener: tokio::net::TcpListener,
    broker: Arc<Broker>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut shutdown => break,
            done = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = done {
                    eprintln!("[broker] connection task failed: {error}");
                }
                continue;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("[broker] accept error: {error}");
                continue;
            }
        };
        let broker = broker.clone();
        connections.spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |request| {
                let broker = broker.clone();
                async move { Ok::<_, std::convert::Infallible>(route(request, broker, peer).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                let message = error.to_string();
                if !message.contains("connection closed") {
                    eprintln!("[broker] serve error: {message}");
                }
            }
        });
    }
    // A drop mid-upload would leave a seal with one copy, so in-flight requests
    // finish rather than being cut off. There is no deadline here on purpose:
    // the longest realistic request is a multi-GB seal, and Kubernetes' own
    // termination grace period is the outer bound.
    while connections.join_next().await.is_some() {}
}

type Body = Full<Bytes>;

fn text(status: StatusCode, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(body.into()))
        .expect("static response")
}

fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(value.to_string())))
        .expect("static response")
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response<Body> {
    json(status, serde_json::json!({ "error": message.to_string() }))
}

async fn route(
    request: Request<Incoming>,
    broker: Arc<Broker>,
    peer: SocketAddr,
) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    match (&method, path.as_str()) {
        (&Method::GET, "/healthz") => return text(StatusCode::OK, "ok"),
        (&Method::GET, "/metrics") => {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                .body(Full::new(Bytes::from(broker.metrics.render())))
                .expect("metrics response")
        }
        _ => {}
    }

    // Everything below this line is tenant-scoped. Identity is established once,
    // before the route is even chosen, so no seal path can be reached without it.
    let tenant = match broker.resolver.resolve(peer).await {
        Ok(tenant) => tenant,
        Err(reason) => {
            eprintln!("[broker] refused {peer}: {reason:#}");
            return error(StatusCode::FORBIDDEN, reason);
        }
    };
    broker.metrics.contact(&tenant);

    match (&method, path.as_str()) {
        (&Method::GET, "/v1/seals") => listing(broker).await,
        (&Method::PUT, _) => match path.strip_prefix("/v1/seals/") {
            Some(key) => upload(request, broker, &tenant, key).await,
            None => error(StatusCode::NOT_FOUND, format!("no route for PUT {path}")),
        },
        _ => error(
            StatusCode::NOT_FOUND,
            format!("no route for {method} {path}"),
        ),
    }
}

/// The store's view, as `<key>\t<size>` — the two fields a client needs to work
/// out its delta, and nothing else.
///
/// Cached briefly: the listing costs ten paginated calls and every tenant asks
/// for the same answer on the same cadence.
async fn listing(broker: Arc<Broker>) -> Response<Body> {
    let mut cache = broker.listing.lock().await;
    if !matches!(cache.as_ref(), Some((_, at)) if at.elapsed() < metrics::LISTING_TTL) {
        match broker.store.list().await {
            Ok(objects) => {
                broker
                    .metrics
                    .observe_store(objects.len() as u64, objects.iter().map(|o| o.size).sum());
                *cache = Some((objects, Instant::now()));
            }
            Err(error_) => {
                eprintln!("[broker] listing failed: {error_:#}");
                return error(StatusCode::BAD_GATEWAY, error_);
            }
        }
    }
    let body: String = cache
        .as_ref()
        .map(|(objects, _)| {
            objects
                .iter()
                .map(|o| format!("{}\t{}\n", o.key, o.size))
                .collect()
        })
        .unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/tab-separated-values; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("listing response")
}

/// Store one seal.
///
/// Idempotent and size-checked against the store itself, not against the
/// client's belief about it: "the key exists" is not sufficient, because a
/// re-sealed session keeps its key and changes its bytes. The check is a
/// key-scoped list rather than a `head-object` because this service holds no
/// `s3:GetObject`.
///
/// The body is spooled to disk before it is stored. That buys three things at
/// the cost of a temp file: an exact length to compare, a single-part upload
/// that needs no multipart permissions, and a size class with no ceiling below
/// S3's own — the two oversized seals that plant's 90 MiB commit cap kept out of
/// git are precisely the objects a size-shaped path keeps skipping.
async fn upload(
    request: Request<Incoming>,
    broker: Arc<Broker>,
    tenant: &Tenant,
    key: &str,
) -> Response<Body> {
    if let Err(reason) = broker.keys.validate(key) {
        return error(StatusCode::BAD_REQUEST, reason);
    }
    if let Some(declared) = content_length(&request) {
        if declared > broker.max_object_bytes {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "{declared} bytes exceeds the {} byte single-object limit",
                    broker.max_object_bytes
                ),
            );
        }
    }

    let spool = broker
        .spool
        .join(format!("seal-{}.upload", uuid::Uuid::new_v4().simple()));
    let result = store_seal(request, &broker, key, &spool).await;
    let _ = tokio::fs::remove_file(&spool).await;

    match result {
        Ok(Stored::Uploaded(size)) => {
            broker.metrics.record(tenant, Outcome::Uploaded, size);
            eprintln!("[broker] {tenant} stored {key} ({size} B)");
            json(
                StatusCode::OK,
                serde_json::json!({"outcome": "uploaded", "key": key, "size": size}),
            )
        }
        Ok(Stored::Unchanged(size)) => {
            broker.metrics.record(tenant, Outcome::Unchanged, size);
            json(
                StatusCode::OK,
                serde_json::json!({"outcome": "unchanged", "key": key, "size": size}),
            )
        }
        Err(reason) => {
            broker.metrics.record(tenant, Outcome::Failed, 0);
            eprintln!("[broker] {tenant} failed {key}: {reason:#}");
            error(StatusCode::BAD_GATEWAY, reason)
        }
    }
}

enum Stored {
    Uploaded(u64),
    Unchanged(u64),
}

async fn store_seal(
    request: Request<Incoming>,
    broker: &Broker,
    key: &str,
    spool: &std::path::Path,
) -> Result<Stored> {
    let declared = content_length(&request);
    let size = spool_body(request.into_body(), spool, broker.max_object_bytes).await?;
    if let Some(declared) = declared {
        if declared != size {
            anyhow::bail!("body is {size} bytes against a declared Content-Length of {declared}");
        }
    }
    if broker.store.size_of(key).await? == Some(size) {
        return Ok(Stored::Unchanged(size));
    }
    broker.store.put(key, spool).await?;
    // The listing is now stale by one object. Dropping it costs one relist on the
    // next reconcile and keeps a client from being told a seal it just stored is
    // still missing.
    *broker.listing.lock().await = None;
    Ok(Stored::Uploaded(size))
}

fn content_length(request: &Request<Incoming>) -> Option<u64> {
    request
        .headers()
        .get(hyper::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Stream a request body to disk, refusing to grow past the limit.
///
/// Frame by frame rather than `collect()`: a 2.7 GB seal is a real object in
/// this corpus and buffering one in the heap would be a memory limit away from
/// killing the pod mid-upload.
///
/// Generic over the body so a test drives the identical loop rather than a
/// hand-copied mirror of it that is free to drift.
async fn spool_body<B>(mut body: B, path: &std::path::Path, limit: u64) -> Result<u64>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| format!("create spool file {}", path.display()))?;
    let mut size = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.context("read the request body")?;
        let Some(chunk) = frame.data_ref() else {
            continue;
        };
        size += chunk.len() as u64;
        if size > limit {
            anyhow::bail!("body exceeds the {limit} byte single-object limit");
        }
        file.write_all(chunk).await.context("spool the seal")?;
    }
    file.flush().await.context("flush the seal spool")?;
    Ok(size)
}

#[cfg(test)]
mod tests;
