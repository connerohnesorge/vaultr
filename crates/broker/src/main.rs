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
//! This is the general plant broker with seal upload and one restricted read
//! route. The shape is justified by tenancy, not by speculation about future
//! routes; widening the read allowlist is a reviewed deployment change. The
//! broker signs reads but never buffers seal bytes, so the Plant process remains
//! outside the seal-store credential boundary.
//!
//! ## Routes
//!
//! | Route | Auth | Purpose |
//! |---|---|---|
//! | `GET /healthz` | open | liveness |
//! | `GET /metrics` | open | Prometheus scrape (counts and ages, no seal content) |
//! | `GET /v1/seals` | tenant | the store's view — `<key>\t<size>` — for reconciling |
//! | `GET /v1/seals/<key>` | CB14957 | redirect to a short-lived presigned read |
//! | `PUT /v1/seals/<key>` | tenant | store one seal, idempotently |
//!
//! `/healthz` and `/metrics` are deliberately open: the scrape comes from
//! Prometheus inside the cluster, which is not a tailnet peer, and neither route
//! discloses anything but counts and ages.

mod metrics;
mod seal;
mod store;
mod tenant;

use anyhow::{bail, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use metrics::{Metrics, Outcome};
use seal::KeyPolicy;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use store::{Object, Store};
use tenant::{Resolver, Tenant};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Mutex};

struct Broker {
    store: Store,
    resolver: Resolver,
    metrics: Metrics,
    keys: KeyPolicy,
    read_tenants: HashSet<Tenant>,
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
    let removed = clean_stale_spool(&spool).await?;
    if removed > 0 {
        eprintln!("[broker] removed {removed} stale spool file(s) after restart");
    }
    let dev_tenant = env("SEAL_BROKER_DEV_TENANT").and_then(|name| Tenant::from_node_name(&name));
    let read_tenants = configured_read_tenants(env("SEAL_BROKER_READ_TENANTS"))?;
    let oidc_identity = match (
        env("SEAL_BROKER_OIDC_SUBJECT"),
        env("SEAL_BROKER_OIDC_TENANT"),
    ) {
        (Some(subject), Some(name)) => {
            let tenant = Tenant::from_node_name(&name).with_context(|| {
                format!("SEAL_BROKER_OIDC_TENANT={name:?} is not a usable tenant label")
            })?;
            Some(tenant::OidcIdentity::new(subject, tenant))
        }
        (None, None) => None,
        _ => bail!("SEAL_BROKER_OIDC_SUBJECT and SEAL_BROKER_OIDC_TENANT must be set together"),
    };
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
    let max_aws_processes = env("SEAL_BROKER_MAX_AWS_PROCESSES")
        .map(|value| {
            value
                .parse::<usize>()
                .context("SEAL_BROKER_MAX_AWS_PROCESSES must be a positive integer")
        })
        .transpose()?
        .unwrap_or(store::DEFAULT_MAX_AWS_PROCESSES);
    if max_aws_processes == 0 {
        bail!("SEAL_BROKER_MAX_AWS_PROCESSES must be a positive integer");
    }
    let drain_timeout = env("SEAL_BROKER_DRAIN_TIMEOUT")
        .map(|value| {
            value
                .parse::<u64>()
                .context("SEAL_BROKER_DRAIN_TIMEOUT must be a positive number of seconds")
        })
        .transpose()?
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_DRAIN_TIMEOUT);
    if drain_timeout.is_zero() {
        bail!("SEAL_BROKER_DRAIN_TIMEOUT must be a positive number of seconds");
    }

    let broker = Arc::new(Broker {
        store: Store::with_max_aws_processes(
            env("SEAL_BROKER_BUCKET").unwrap_or_else(|| store::DEFAULT_BUCKET.into()),
            max_aws_processes,
        ),
        resolver: Resolver::new(env("SEAL_BROKER_TAILSCALE_SOCKET"), dev_tenant)
            .with_oidc(oidc_identity)
            .with_kube(
                tenant::kube::Config::from_env(env)
                    .map(tenant::kube::Identities::new)
                    .transpose()?,
            ),
        metrics: Metrics::new(),
        keys: KeyPolicy::new(seal_files),
        read_tenants,
        spool,
        max_object_bytes: env("SEAL_BROKER_MAX_OBJECT_BYTES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(store::MAX_OBJECT_BYTES),
        listing: Mutex::new(None),
    });

    eprintln!(
        "[broker] store s3://{} seals {} read tenants {} spool {} max AWS processes {}",
        broker.store.bucket(),
        broker.keys.seal_files().join(","),
        if broker.read_tenants.is_empty() {
            "NONE".to_string()
        } else {
            broker
                .read_tenants
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        },
        broker.spool.display(),
        max_aws_processes
    );
    if let Some(oidc) = broker.resolver.oidc() {
        eprintln!(
            "[broker] identity leg: private gateway OIDC subject bound to tenant {}",
            oidc.tenant()
        );
    }
    // Loud, every start, because it is the one setting that lets a caller be a
    // tenant without proving anything. It covers loopback only, and it must never
    // be quietly true in athens.
    // Which legs can name a caller, said out loud at every start. A broker that
    // quietly lost the TokenReview or gateway leg would look perfectly healthy
    // while seals pile up with no second copy and no alert naming the cause.
    let mut identity_legs = Vec::new();
    if broker.resolver.oidc().is_some() {
        identity_legs.push("private gateway OIDC".to_string());
    }
    if let Some(kube) = broker.resolver.kube() {
        identity_legs.push(format!(
            "projected SA tokens for audience {} from namespace {}",
            kube.audience(),
            kube.namespace()
        ));
    }
    if broker.resolver.tailnet_enabled() {
        identity_legs.push("tailnet whois".to_string());
    }
    eprintln!(
        "[broker] identity legs enabled: {}",
        if identity_legs.is_empty() {
            "NONE".to_string()
        } else {
            identity_legs.join(", ")
        }
    );
    if let Some(dev) = broker.resolver.dev_tenant() {
        eprintln!(
            "[broker] WARNING: SEAL_BROKER_DEV_TENANT={dev} — loopback callers are \
             accepted as this tenant without tailnet identity. Local proving only."
        );
    }

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    eprintln!(
        "[broker] tenant API listening on {}",
        listener.local_addr()?
    );

    // Keep the tenant API and observability surface on separate listeners so a
    // ClusterIP Service can scrape health and metrics without creating an
    // unintended route to `/v1`.
    let observability = match env("SEAL_BROKER_METRICS_LISTEN") {
        Some(value) => {
            let addr: SocketAddr = value
                .parse()
                .context("SEAL_BROKER_METRICS_LISTEN must be <addr>:<port>")?;
            let metrics_listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind metrics listener {addr}"))?;
            eprintln!(
                "[broker] health and metrics listening on {}",
                metrics_listener.local_addr()?
            );
            let metrics_broker = broker.clone();
            Some(tokio::spawn(serve(
                metrics_listener,
                metrics_broker,
                std::future::pending(),
                Surface::Observability,
                drain_timeout,
            )))
        }
        None => None,
    };

    eprintln!(
        "[broker] graceful drain timeout {}s",
        drain_timeout.as_secs()
    );
    serve(
        listener,
        broker,
        shutdown(),
        Surface::TenantApi,
        drain_timeout,
    )
    .await;
    if let Some(task) = observability {
        task.abort();
        let _ = task.await;
    }
    Ok(())
}

async fn clean_stale_spool(spool: &std::path::Path) -> Result<usize> {
    let mut entries = tokio::fs::read_dir(spool)
        .await
        .with_context(|| format!("read the seal spool directory {}", spool.display()))?;
    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await.context("read spool entry")? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("seal-")
            && name.ends_with(".upload")
            && entry
                .file_type()
                .await
                .context("read spool entry type")?
                .is_file()
        {
            tokio::fs::remove_file(entry.path())
                .await
                .with_context(|| format!("remove stale spool file {}", entry.path().display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn configured_read_tenants(value: Option<String>) -> Result<HashSet<Tenant>> {
    let Some(value) = value else {
        return Ok(HashSet::new());
    };

    value
        .split(',')
        .map(str::trim)
        .map(|name| {
            if name.is_empty() {
                bail!("SEAL_BROKER_READ_TENANTS contains an empty tenant");
            }
            Tenant::from_node_name(name).with_context(|| {
                format!("SEAL_BROKER_READ_TENANTS contains unusable tenant {name:?}")
            })
        })
        .collect()
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

const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(540);

#[derive(Clone, Copy)]
enum Surface {
    TenantApi,
    Observability,
}

async fn serve(
    listener: tokio::net::TcpListener,
    broker: Arc<Broker>,
    shutdown: impl std::future::Future<Output = ()>,
    surface: Surface,
    drain_timeout: Duration,
) {
    tokio::pin!(shutdown);
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut shutdown => {
                // Stop accepting first, then tell every existing HTTP/1
                // connection to disable keep-alive. Without this signal an idle
                // Envoy upstream connection can keep `serve_connection` pending
                // until Kubernetes kills the pod at the full grace boundary.
                let _ = shutdown_tx.send(());
                break;
            }
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
        let mut shutdown_rx = shutdown_tx.subscribe();
        connections.spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |request| {
                let broker = broker.clone();
                async move {
                    let response = match surface {
                        Surface::TenantApi => route(request, broker, peer).await,
                        Surface::Observability => observability_route(request, broker),
                    };
                    Ok::<_, std::convert::Infallible>(response)
                }
            });
            let mut connection =
                Box::pin(hyper::server::conn::http1::Builder::new().serve_connection(io, service));
            tokio::select! {
                result = &mut connection => report_connection_result(result),
                _ = shutdown_rx.recv() => {
                    connection.as_mut().graceful_shutdown();
                    report_connection_result(connection.await);
                }
            }
        });
    }

    // In-flight uploads get the full bounded drain window, while idle keep-alive
    // connections close immediately after the broadcast above. A malicious or
    // broken client that never completes a body cannot hold Recreate hostage
    // forever; abort only after the window that fits inside Kubernetes' grace.
    if tokio::time::timeout(drain_timeout, async {
        while let Some(result) = connections.join_next().await {
            if let Err(error) = result {
                eprintln!("[broker] connection task failed during drain: {error}");
            }
        }
    })
    .await
    .is_err()
    {
        eprintln!(
            "[broker] graceful drain timed out after {}s; aborting remaining connections",
            drain_timeout.as_secs()
        );
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}

fn report_connection_result(result: Result<(), hyper::Error>) {
    if let Err(error) = result {
        let message = error.to_string();
        if !message.contains("connection closed") {
            eprintln!("[broker] serve error: {message}");
        }
    }
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

fn observability_route(request: Request<Incoming>, broker: Arc<Broker>) -> Response<Body> {
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/healthz") => text(StatusCode::OK, "ok"),
        (&Method::GET, "/metrics") => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
            .body(Full::new(Bytes::from(broker.metrics.render())))
            .expect("metrics response"),
        _ => error(
            StatusCode::NOT_FOUND,
            "the observability listener serves only GET /healthz and GET /metrics",
        ),
    }
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
    let gateway_subject = match gateway_subject(&request) {
        Ok(subject) => subject,
        Err(reason) => {
            eprintln!("[broker] refused {peer}: {reason:#}");
            return error(StatusCode::FORBIDDEN, reason);
        }
    };
    let bearer = bearer_token(&request);
    let tenant = match broker
        .resolver
        .resolve(peer, bearer.as_deref(), gateway_subject.as_deref())
        .await
    {
        Ok(tenant) => tenant,
        Err(reason) => {
            eprintln!("[broker] refused {peer}: {reason:#}");
            return error(StatusCode::FORBIDDEN, reason);
        }
    };
    broker.metrics.contact(&tenant);

    match (&method, path.as_str()) {
        (&Method::GET, "/v1/seals") => listing(broker).await,
        (&Method::GET, _) => match path.strip_prefix("/v1/seals/") {
            Some(key) => read_seal(broker, &tenant, key).await,
            None => error(StatusCode::NOT_FOUND, format!("no route for GET {path}")),
        },
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

const PRESIGNED_URL_EXPIRES_SECONDS: u64 = 300;

/// Return a short-lived redirect to one seal object.
///
/// Read access is a separate allowlist from upload access. The caller must be
/// authenticated before this function runs, and the key is validated before the
/// broker asks the store to sign anything.
async fn read_seal(broker: Arc<Broker>, tenant: &Tenant, key: &str) -> Response<Body> {
    if !broker.read_tenants.contains(tenant) {
        return error(
            StatusCode::FORBIDDEN,
            format!("tenant {tenant} is not authorized to read seals"),
        );
    }
    if let Err(reason) = broker.keys.validate(key) {
        return error(StatusCode::BAD_REQUEST, reason);
    }

    match broker
        .store
        .presign(key, PRESIGNED_URL_EXPIRES_SECONDS)
        .await
    {
        Ok(url) => Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(hyper::header::LOCATION, url)
            .header(hyper::header::CACHE_CONTROL, "no-store")
            .body(Full::new(Bytes::new()))
            .expect("presigned seal redirect"),
        Err(reason) => {
            eprintln!("[broker] {tenant} failed to sign {key}: {reason:#}");
            error(StatusCode::BAD_GATEWAY, reason)
        }
    }
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

/// The gateway-projected OIDC subject, if the caller offered one.
///
/// An invalid header is an authentication error rather than an absent header:
/// silently falling through to another identity leg would make malformed or
/// forged gateway metadata ambiguous in the logs and policy.
fn gateway_subject(request: &Request<Incoming>) -> Result<Option<String>> {
    let Some(value) = request.headers().get(tenant::GATEWAY_SUBJECT_HEADER) else {
        return Ok(None);
    };
    let subject = value
        .to_str()
        .context("gateway identity header is not valid UTF-8")?
        .trim();
    if subject.is_empty() {
        bail!("gateway identity header is empty");
    }
    Ok(Some(subject.to_owned()))
}

/// The `Authorization: Bearer` value, if the caller offered one.
///
/// Returned owned rather than borrowed because the request is consumed by the
/// upload path below, and deliberately never logged: it is somebody's live
/// credential for as long as their pod exists.
fn bearer_token(request: &Request<Incoming>) -> Option<String> {
    let value = request
        .headers()
        .get(hyper::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    // The scheme is case-insensitive per RFC 7235; a client that sends `bearer`
    // is not making a mistake worth a 403 it cannot diagnose.
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
        .map(String::from)
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
