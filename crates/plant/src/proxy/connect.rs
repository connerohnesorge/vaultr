//! Forward-proxy leg: `CONNECT` tunnelling, with TLS interception for the one
//! upstream host this adapter proxies.
//!
//! Claude Code >= 2.1.196 refuses Remote Control when `ANTHROPIC_BASE_URL` names
//! a host other than `api.anthropic.com`, and there is no override. So the
//! client is pointed at Plant with `HTTPS_PROXY` instead, leaving the base URL
//! untouched. Everything the client reaches — claude.ai, platform.claude.com,
//! the Datadog intakes — arrives here as CONNECT. Only the adapter's own
//! upstream is decrypted; the rest is spliced through byte-for-byte.
//!
//! The origin-form reverse-proxy path in the parent module is untouched, so an
//! `ANTHROPIC_BASE_URL` client keeps working exactly as before.

use super::{full, BoxBody, CaptureTasks, ProxyCtx};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

/// `host:port` from a CONNECT request-target (authority-form).
fn authority(req: &Request<hyper::body::Incoming>) -> Option<(String, u16)> {
    let authority = req.uri().authority()?;
    let host = authority.host();
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), authority.port_u16().unwrap_or(443)))
}

pub(super) async fn handle(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<ProxyCtx>,
    capture_tasks: CaptureTasks,
) -> Response<BoxBody> {
    let Some((host, port)) = authority(&req) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(full("CONNECT requires an authority-form target\n"))
            .unwrap();
    };

    let intercept = ctx.adapter.interceptable_host() == Some(host.as_str());

    // The upgrade future only resolves once this 200 has been written, so the
    // work is spawned and the response returned immediately.
    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(upgraded) => TokioIo::new(upgraded),
            Err(error) => {
                eprintln!("[connect] upgrade failed for {host}:{port}: {error}");
                return;
            }
        };
        let outcome = if intercept {
            intercept_tls(upgraded, &host, ctx, capture_tasks).await
        } else {
            splice(upgraded, &host, port).await
        };
        if let Err(error) = outcome {
            // A client hanging up mid-tunnel is routine.
            let msg = error.to_string();
            if !msg.contains("connection reset") && !msg.contains("broken pipe") {
                eprintln!("[connect] {host}:{port}: {msg}");
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .body(full(Vec::new()))
        .unwrap()
}

/// Blind byte splice. Plant learns the hostname and nothing else — no
/// certificate is minted, so the client validates the real server as usual.
async fn splice<S>(mut client: S, host: &str, port: u16) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut server = TcpStream::connect((host, port)).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

/// Terminate TLS with a leaf minted for `host`, then run the ordinary request
/// handler over it. After the handshake the requests are origin-form
/// (`POST /v1/messages`), which is exactly what the reverse-proxy path already
/// expects, so capture needs no special case.
async fn intercept_tls<S>(
    client: S,
    host: &str,
    ctx: Arc<ProxyCtx>,
    capture_tasks: CaptureTasks,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let config = ctx.ca.server_config(host)?;
    let tls = TlsAcceptor::from(config).accept(client).await?;

    let service = hyper::service::service_fn(move |req| {
        let ctx = ctx.clone();
        let capture_tasks = capture_tasks.clone();
        async move { Ok::<_, Infallible>(super::handle_origin(req, ctx, capture_tasks).await) }
    });

    // No idle timeout, same as the plaintext listener: the Remote Control
    // control channel is a long-lived SSE stream on this same connection, and
    // a >10 min gap ends the remote session.
    if let Err(error) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls), service)
        .with_upgrades()
        .await
    {
        let msg = error.to_string();
        if !msg.contains("connection closed") {
            return Err(std::io::Error::other(msg));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::adapter::adapters;
    use crate::domain::Harness;

    #[test]
    fn anthropic_upstream_is_interceptable_codex_is_not() {
        let all = adapters();
        let anthropic = all
            .iter()
            .find(|a| a.harness == Harness::ClaudeCode)
            .unwrap();
        let codex = all.iter().find(|a| a.harness == Harness::Codex).unwrap();

        assert_eq!(anthropic.interceptable_host(), Some("api.anthropic.com"));
        // Path-carrying base: intercepting would re-append the prefix.
        assert_eq!(codex.interceptable_host(), None);
    }

    #[test]
    fn plain_http_upstream_with_port_yields_bare_host() {
        let mut adapter = adapters()
            .into_iter()
            .find(|a| a.harness == Harness::ClaudeCode)
            .unwrap();
        adapter.upstream = "http://127.0.0.1:9931".to_string();
        assert_eq!(adapter.interceptable_host(), Some("127.0.0.1"));
    }
}
