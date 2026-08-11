//! Who is calling: the tenant is the calling headscale node, and nothing else.
//!
//! There is no bearer token, no shared secret and no OIDC leg here, which is the
//! point rather than an omission. A per-tenant secret is a durable client
//! credential living on the very boxes the broker exists to keep credentials off,
//! and the cnb OIDC refresher has been dead since 2026-07-24 — depending on it
//! would mean depending on a component whose failure mode is the silent stall
//! this design is built to avoid. Tailnet identity holds no expiring secret on
//! the client at all, and it already carries `cnb computer shell`, the most
//! sensitive path in this effort.
//!
//! Identity comes from the local `tailscaled`'s own view of the tailnet, over
//! its unix socket. That is a read of the netmap the control server already
//! pushed down: no API key, no call to headscale, nothing to rotate.

use anyhow::{bail, Context, Result};
use http_body_util::BodyExt;
use hyper::Request;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Where `tailscaled` listens for local API calls inside a pod that shares its
/// network namespace. Overridable because the socket moves per platform — on the
/// Mac it is `/var/run/tailscaled.socket`.
pub const DEFAULT_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// Tailscale's CGNAT range. An address outside it did not arrive over the
/// tailnet, whatever it claims.
const TAILNET_V4: (u8, u8) = (100, 64);

/// How long one peer's identity is trusted before it is looked up again. A
/// reconcile hands over dozens of seals on one connection burst; re-asking
/// `tailscaled` per object buys nothing.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// A tenant of the broker: one headscale node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tenant(String);

impl Tenant {
    /// Node names arrive fully qualified and cased (`CB14957.hs.cnb.rocks.`),
    /// and become a Prometheus label, so they are reduced to the short name in
    /// the label character set. Anything outside it is dropped rather than
    /// escaped: a name that survives is a name that reads the same in a query,
    /// an alert and a log line.
    pub fn from_node_name(name: &str) -> Option<Self> {
        let short: String = name
            .trim_matches('.')
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        (!short.is_empty()).then_some(Tenant(short))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Tenant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// True for an address `tailscaled` could plausibly have handed us.
///
/// This is a prefilter, not the authorisation: the lookup below is what actually
/// decides, and it fails closed for an address the tailnet does not know. Its
/// job is to keep a cluster-internal pod IP from ever reaching the socket.
pub fn is_tailnet_addr(ip: IpAddr) -> bool {
    match ip {
        // 100.64.0.0/10
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == TAILNET_V4.0 && (TAILNET_V4.1..128).contains(&b)
        }
        // fd7a:115c:a1e0::/48 — Tailscale's ULA range.
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

pub struct Resolver {
    socket: String,
    /// Set only when the broker is running outside the tailnet on purpose —
    /// local proving. Announced loudly at startup; never a default.
    dev_tenant: Option<Tenant>,
    cache: Mutex<HashMap<IpAddr, (Tenant, Instant)>>,
}

impl Resolver {
    pub fn new(socket: impl Into<String>, dev_tenant: Option<Tenant>) -> Self {
        Resolver {
            socket: socket.into(),
            dev_tenant,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn dev_tenant(&self) -> Option<&Tenant> {
        self.dev_tenant.as_ref()
    }

    /// The tenant behind a connection, or an error naming why it is nobody.
    pub async fn resolve(&self, peer: std::net::SocketAddr) -> Result<Tenant> {
        if let Some(tenant) = &self.dev_tenant {
            if peer.ip().is_loopback() {
                return Ok(tenant.clone());
            }
        }
        if !is_tailnet_addr(peer.ip()) {
            bail!(
                "{} is not a tailnet address; the broker serves tailnet peers only",
                peer.ip()
            );
        }
        if let Some(hit) = self.cached(peer.ip()) {
            return Ok(hit);
        }
        let node = self.whois(peer).await?;
        let tenant = Tenant::from_node_name(&node).with_context(|| {
            format!("tailscaled named the peer {node:?}, which yields no usable tenant")
        })?;
        self.cache
            .lock()
            .unwrap()
            .insert(peer.ip(), (tenant.clone(), Instant::now()));
        Ok(tenant)
    }

    fn cached(&self, ip: IpAddr) -> Option<Tenant> {
        let mut cache = self.cache.lock().unwrap();
        match cache.get(&ip) {
            Some((tenant, at)) if at.elapsed() < CACHE_TTL => Some(tenant.clone()),
            Some(_) => {
                cache.remove(&ip);
                None
            }
            None => None,
        }
    }

    /// Ask the local daemon who owns an address.
    async fn whois(&self, peer: std::net::SocketAddr) -> Result<String> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .with_context(|| {
                format!(
                    "connect to tailscaled at {} — the broker derives every tenant from it, \
                     so it cannot serve without it",
                    self.socket
                )
            })?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .context("handshake with the tailscaled local API")?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .uri(format!("/localapi/v0/whois?addr={peer}"))
            // The local API refuses a request whose Host it does not recognise,
            // which is its guard against a browser being talked into calling it.
            .header("Host", "local-tailscaled.sock")
            .body(String::new())
            .context("build the whois request")?;
        let response = sender
            .send_request(request)
            .await
            .context("call the tailscaled local API")?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .context("read the whois response")?
            .to_bytes();
        if !status.is_success() {
            bail!(
                "tailscaled does not know {}: {status} {}",
                peer.ip(),
                String::from_utf8_lossy(&body).trim()
            );
        }
        parse_whois(&body)
    }
}

#[derive(Deserialize)]
struct WhoIs {
    #[serde(rename = "Node")]
    node: WhoIsNode,
}

#[derive(Deserialize)]
struct WhoIsNode {
    #[serde(rename = "Name")]
    name: String,
}

fn parse_whois(body: &[u8]) -> Result<String> {
    let whois: WhoIs = serde_json::from_slice(body).context("parse the whois response")?;
    if whois.node.name.trim().is_empty() {
        bail!("whois returned a node with no name");
    }
    Ok(whois.node.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn the_tailnet_range_is_the_whole_cgnat_block() {
        // The Mac, as headscale has it.
        assert!(is_tailnet_addr(Ipv4Addr::new(100, 64, 0, 2).into()));
        assert!(is_tailnet_addr(Ipv4Addr::new(100, 127, 255, 255).into()));
        assert!(is_tailnet_addr(
            "fd7a:115c:a1e0::1".parse::<Ipv6Addr>().unwrap().into()
        ));
        // A pod IP and a loopback are the two ways a non-tailnet caller
        // realistically arrives, and neither is a tenant.
        assert!(!is_tailnet_addr(Ipv4Addr::new(10, 0, 4, 17).into()));
        assert!(!is_tailnet_addr(Ipv4Addr::LOCALHOST.into()));
        // 100.63 and 100.128 bracket the block; both are ordinary public space.
        assert!(!is_tailnet_addr(Ipv4Addr::new(100, 63, 0, 1).into()));
        assert!(!is_tailnet_addr(Ipv4Addr::new(100, 128, 0, 1).into()));
    }

    #[test]
    fn a_node_name_reduces_to_a_label_safe_short_name() {
        assert_eq!(
            Tenant::from_node_name("CB14957.hs.cnb.rocks.").unwrap().0,
            "cb14957"
        );
        assert_eq!(Tenant::from_node_name("dev-box").unwrap().0, "dev-box");
        // Nothing usable is left, so nothing is invented.
        assert!(Tenant::from_node_name("").is_none());
        assert!(Tenant::from_node_name("...").is_none());
        assert!(Tenant::from_node_name("!!!.hs.cnb.rocks.").is_none());
    }

    #[test]
    fn whois_yields_the_node_name() {
        let body = br#"{"Node":{"Name":"cb14957.hs.cnb.rocks.","ID":7},
                       "UserProfile":{"LoginName":"cohnesor@"}}"#;
        assert_eq!(parse_whois(body).unwrap(), "cb14957.hs.cnb.rocks.");
        // A node the daemon cannot name is not a tenant with an empty name.
        assert!(parse_whois(br#"{"Node":{"Name":"  "}}"#).is_err());
        assert!(parse_whois(b"not json").is_err());
    }

    #[tokio::test]
    async fn a_non_tailnet_peer_is_refused_before_the_socket_is_touched() {
        // The socket path is deliberately nonexistent: reaching it would be the
        // bug. A cluster peer must be refused by address, not by lookup failure.
        let resolver = Resolver::new("/nonexistent/tailscaled.sock", None);
        let error = resolver
            .resolve("10.0.4.17:44120".parse().unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a tailnet address"), "{error}");
    }

    #[tokio::test]
    async fn the_dev_tenant_covers_loopback_only() {
        let resolver = Resolver::new(
            "/nonexistent/tailscaled.sock",
            Some(Tenant("localdev".into())),
        );
        assert_eq!(
            resolver
                .resolve("127.0.0.1:5555".parse().unwrap())
                .await
                .unwrap()
                .as_str(),
            "localdev"
        );
        // Off loopback the dev escape hatch must not widen anything.
        assert!(resolver
            .resolve("10.0.4.17:5555".parse().unwrap())
            .await
            .is_err());
    }
}
