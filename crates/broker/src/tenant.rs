//! Who is calling: a tenant is one machine, proved rather than asserted.
//!
//! There are three legs, and none is a durable client secret. That constraint is
//! the point rather than an omission: a per-tenant secret is a credential living
//! on the very boxes the broker exists to keep credentials off.
//!
//! - **Gateway OIDC subject** (the Mac after the #1328 cutover). Envoy Gateway
//!   verifies the Dex bearer, strips any client-supplied identity header, and
//!   injects the verified `sub` claim. The broker accepts that subject only when
//!   it matches the configured owner binding, then maps it to the stable
//!   `cb14957` audit tenant. The subject is therefore evidence from the gateway,
//!   not a caller-selected tenant name.
//! - **Tailnet** (the Mac during the no-gap transition). Identity comes from the
//!   local `tailscaled`'s own view of the tailnet, over its unix socket. This is
//!   retained until the HTTPS path has carried live traffic and is then removed.
//! - **Projected ServiceAccount token** (a vended computer, #1419). The caller
//!   presents the token its kubelet projected for it and the broker resolves it
//!   by **TokenReview against the API server** — never by trusting a claim in the
//!   request. The kubelet rotates it, its lifetime is bounded by the pod's, it is
//!   audience-bound to this service, and it is worthless outside the cluster. See
//!   [`kube`].
//!
//! A gateway subject is resolved by the first leg, a bearer token by the third,
//! and a request with neither falls back to tailnet whois during the transition.
//! All three fail closed, and the gateway leg cannot name a tenant other than the
//! one explicitly bound at deployment time.

pub mod kube;

use anyhow::{bail, Context, Result};
use http_body_util::BodyExt;
use hyper::Request;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Header injected by the private Envoy Gateway after it verifies the Dex JWT.
/// Envoy's claim projection overwrites any client-supplied value before the
/// broker sees it, so the broker never treats a caller-selected subject as proof.
pub const GATEWAY_SUBJECT_HEADER: &str = "x-plant-caller-sub";

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
        Self::from_label_source(name.trim_matches('.').split('.').next().unwrap_or_default())
    }

    /// A pod name is already lowercase and label-safe by the API server's own
    /// validation, so this is a belt-and-braces pass rather than a reduction —
    /// but it is the same pass, so a tenant reads identically whichever leg
    /// named it. Unlike a node name it is *not* split on `.`: a pod name has no
    /// domain suffix to strip, and splitting one would silently truncate.
    pub fn from_pod_name(name: &str) -> Option<Self> {
        Self::from_label_source(name)
    }

    fn from_label_source(raw: &str) -> Option<Self> {
        let short: String = raw
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
/// This is a prefilter on **the tailnet leg only**, not the authorisation: the
/// whois below is what actually decides, and it fails closed for an address the
/// tailnet does not know. Its job is to keep a cluster-internal pod IP from
/// reaching the `tailscaled` socket to ask a question it can only answer "no" to.
///
/// It was a global prefilter until #1419. It stopped being one because a vended
/// computer *is* a cluster-internal pod IP and is now a legitimate caller — but
/// it arrives on the token leg, which is not gated on address at all, so nothing
/// here loosened: an address outside the tailnet with no token is refused exactly
/// as before.
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

/// The one gateway caller binding currently needed by the live Mac. Keeping the
/// subject-to-tenant mapping explicit preserves the historical `cb14957` audit
/// key while still making the verified OIDC subject part of the authentication
/// decision. Adding another caller is a reviewed binding change, not an implicit
/// expansion of a header-derived namespace.
#[derive(Clone)]
pub struct OidcIdentity {
    expected_subject: String,
    tenant: Tenant,
}

impl OidcIdentity {
    pub fn new(expected_subject: impl Into<String>, tenant: Tenant) -> Self {
        Self {
            expected_subject: expected_subject.into(),
            tenant,
        }
    }

    pub fn tenant(&self) -> &Tenant {
        &self.tenant
    }

    fn resolve(&self, subject: &str) -> Result<Tenant> {
        if subject == self.expected_subject {
            return Ok(self.tenant.clone());
        }
        bail!("gateway OIDC subject is not authorized for this broker")
    }
}

pub struct Resolver {
    /// The legacy tailnet whois leg is opt-in. Production gateway deployments
    /// leave this `None`, so an unmatched request cannot probe a dead socket.
    socket: Option<String>,
    /// The gateway OIDC leg, when this deployment exposes the private HTTPS
    /// route. The subject is accepted only through the explicit binding above.
    oidc: Option<OidcIdentity>,
    /// The projected-SA-token leg, when the broker is running in a cluster that
    /// can answer a TokenReview. `None` outside one — tests, local proving — in
    /// which case a bearer token is refused rather than half-checked.
    kube: Option<kube::Identities>,
    /// Set only when the broker is running outside the tailnet on purpose —
    /// local proving. Announced loudly at startup; never a default.
    dev_tenant: Option<Tenant>,
    cache: Mutex<HashMap<IpAddr, (Tenant, Instant)>>,
}

impl Resolver {
    pub fn new(socket: Option<String>, dev_tenant: Option<Tenant>) -> Self {
        Resolver {
            socket,
            oidc: None,
            kube: None,
            dev_tenant,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn tailnet_enabled(&self) -> bool {
        self.socket.is_some()
    }

    pub fn with_oidc(mut self, oidc: Option<OidcIdentity>) -> Self {
        self.oidc = oidc;
        self
    }

    pub fn oidc(&self) -> Option<&OidcIdentity> {
        self.oidc.as_ref()
    }

    pub fn with_kube(mut self, kube: Option<kube::Identities>) -> Self {
        self.kube = kube;
        self
    }

    pub fn dev_tenant(&self) -> Option<&Tenant> {
        self.dev_tenant.as_ref()
    }

    pub fn kube(&self) -> Option<&kube::Identities> {
        self.kube.as_ref()
    }

    /// The tenant behind a request, or an error naming why it is nobody.
    ///
    /// `bearer` is the request's `Authorization: Bearer` value if it carried one.
    /// Its presence is what selects the leg: a caller offering a token is asking
    /// to be identified by it and gets exactly that answer, pass or fail, rather
    /// than a silent fallback to being judged by its address.
    pub async fn resolve(
        &self,
        peer: std::net::SocketAddr,
        bearer: Option<&str>,
        gateway_subject: Option<&str>,
    ) -> Result<Tenant> {
        if let Some(subject) = gateway_subject {
            // Envoy Gateway may leave the already-validated OIDC bearer on the
            // upstream request. The projected-identity branch is selected by
            // the claim header, so that bearer is not mistaken for a Kubernetes
            // TokenReview token (its `pantheon-cli` audience is intentionally
            // unrelated to `plant-broker`).
            let Some(oidc) = &self.oidc else {
                bail!(
                    "this broker has no gateway OIDC leg configured, so a forwarded subject \
                     is refused rather than assumed"
                );
            };
            return oidc.resolve(subject);
        }
        if let Some(token) = bearer {
            let Some(kube) = &self.kube else {
                bail!(
                    "this broker has no TokenReview leg configured, so a bearer token \
                     cannot be checked and is refused rather than assumed"
                );
            };
            return kube.resolve(token).await;
        }
        if let Some(tenant) = &self.dev_tenant {
            if peer.ip().is_loopback() {
                return Ok(tenant.clone());
            }
        }
        let Some(_) = self.socket.as_deref() else {
            bail!(
                "tailnet identity is disabled and the request carried neither a gateway \
                 subject nor a ServiceAccount token"
            );
        };
        if !is_tailnet_addr(peer.ip()) {
            bail!(
                "{} is not a tailnet address and the request carried no ServiceAccount \
                 token; the broker has no other way to name this caller",
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
        let socket = self
            .socket
            .as_deref()
            .context("tailnet identity is disabled; no tailscaled socket is configured")?;
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| {
                format!(
                    "connect to tailscaled at {socket} — the broker derives every tenant from it, \
                     so it cannot serve without it"
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    #[test]
    fn a_pod_name_keeps_every_segment_a_node_name_would_have_lost() {
        // `from_node_name` strips the domain; a pod name has none, and the part
        // after a `-` is the ordinal that distinguishes two computers.
        assert_eq!(
            Tenant::from_pod_name("computer-cohnesor-scratch-0")
                .unwrap()
                .0,
            "computer-cohnesor-scratch-0"
        );
        assert!(Tenant::from_pod_name("").is_none());
    }

    #[tokio::test]
    async fn the_gateway_subject_maps_to_the_stable_mac_tenant_only_when_bound() {
        let resolver = Resolver::new(None, None).with_oidc(Some(OidcIdentity::new(
            "owner-subject",
            Tenant("cb14957".into()),
        )));
        assert!(!resolver.tailnet_enabled());
        assert_eq!(
            resolver
                .resolve(
                    "10.0.4.17:44120".parse().unwrap(),
                    None,
                    Some("owner-subject"),
                )
                .await
                .unwrap()
                .as_str(),
            "cb14957"
        );
        let error = resolver
            .resolve(
                "10.0.4.17:44120".parse().unwrap(),
                None,
                Some("another-subject"),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not authorized"), "{error}");
    }

    #[tokio::test]
    async fn a_projected_service_account_resolves_with_tailnet_disabled() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let body = serde_json::json!({
                "apiVersion": "authentication.k8s.io/v1",
                "kind": "TokenReview",
                "status": {
                    "authenticated": true,
                    "audiences": ["plant-broker"],
                    "user": {
                        "username": "system:serviceaccount:computers:computer-agent",
                        "extra": {
                            "authentication.kubernetes.io/pod-name": ["computer-oidc-disabled-0"]
                        }
                    }
                }
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "broker-review-token").unwrap();
        let identities = kube::Identities::for_test(
            format!("http://{addr}/tokenreviews"),
            token_path,
            "plant-broker",
            "computers",
        );
        let resolver = Resolver::new(None, None).with_kube(Some(identities));
        assert!(!resolver.tailnet_enabled());
        let tenant = resolver
            .resolve(
                "10.0.4.17:44120".parse().unwrap(),
                Some("projected-caller-token"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(tenant.as_str(), "computer-oidc-disabled-0");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_gateway_subject_wins_when_the_gateway_forwards_the_oidc_bearer() {
        let resolver = Resolver::new(None, None).with_oidc(Some(OidcIdentity::new(
            "owner-subject",
            Tenant("cb14957".into()),
        )));
        assert!(!resolver.tailnet_enabled());
        assert_eq!(
            resolver
                .resolve(
                    "10.0.4.17:44120".parse().unwrap(),
                    Some("pantheon-cli-token"),
                    Some("owner-subject"),
                )
                .await
                .unwrap()
                .as_str(),
            "cb14957"
        );
    }

    #[tokio::test]
    async fn a_peer_without_identity_is_refused_without_a_tailnet_socket() {
        // Tailnet is explicitly disabled: reaching a socket would be the bug.
        let resolver = Resolver::new(None, None);
        let error = resolver
            .resolve("10.0.4.17:44120".parse().unwrap(), None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("tailnet identity is disabled"), "{error}");
    }

    // The narrowing #1419 asked for, stated as a test: a pod IP is no longer
    // rejected on sight. It is sent to the token leg, and refused there when
    // there is no leg — never accepted, and never bounced on its address.
    #[tokio::test]
    async fn a_pod_ip_bearing_a_token_is_refused_by_the_token_leg_not_by_its_address() {
        let resolver = Resolver::new(None, None);
        let error = resolver
            .resolve("10.0.4.17:44120".parse().unwrap(), Some("a.b.c"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no TokenReview leg"), "{error}");
        assert!(!error.contains("not a tailnet address"), "{error}");
    }

    // A token is a request to be judged by the token, so a tailnet peer that
    // sends a bad one gets refused rather than quietly falling back to whois and
    // being named by an address it may not control.
    #[tokio::test]
    async fn a_token_is_never_downgraded_to_the_address_leg() {
        let resolver = Resolver::new(
            Some("/nonexistent/tailscaled.sock".into()),
            Some(Tenant("localdev".into())),
        );
        assert!(resolver
            .resolve("100.64.0.2:5555".parse().unwrap(), Some("junk"), None)
            .await
            .is_err());
        assert!(resolver
            .resolve("127.0.0.1:5555".parse().unwrap(), Some("junk"), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_dev_tenant_covers_loopback_only() {
        let resolver = Resolver::new(
            Some("/nonexistent/tailscaled.sock".into()),
            Some(Tenant("localdev".into())),
        );
        assert_eq!(
            resolver
                .resolve("127.0.0.1:5555".parse().unwrap(), None, None)
                .await
                .unwrap()
                .as_str(),
            "localdev"
        );
        // Off loopback the dev escape hatch must not widen anything.
        assert!(resolver
            .resolve("10.0.4.17:5555".parse().unwrap(), None, None)
            .await
            .is_err());
    }
}
