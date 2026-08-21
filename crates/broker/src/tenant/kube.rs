//! The projected-ServiceAccount-token identity leg (#1419).
//!
//! A vended computer has no tailnet — headscale is being removed from athens
//! entirely — so it cannot be named by the netmap. What it does have is a token
//! its own kubelet projected for it, and an API server that will say, on the
//! record, exactly whose it is. That answer is the tenant.
//!
//! ## Nothing here trusts the caller
//!
//! The token is an opaque string to this module. It is never parsed, and no
//! claim inside it is read: every field the tenant is built from comes back from
//! the API server's own [TokenReview] verdict. A guest that mints a plausible
//! JWT gets `authenticated: false` and a 403, and a guest replaying another
//! pod's token gets that pod's identity — which is why the token must be
//! audience-bound (below) and why the transport is a NetworkPolicy'd ClusterIP
//! rather than anything reachable from outside the cluster.
//!
//! ## Three checks, all of them fail-closed
//!
//! 1. **Audience.** The review is submitted with this broker's audience, so the
//!    API server itself rejects a token minted for anything else — most
//!    importantly, the ordinary kube-apiserver token. The consequence runs the
//!    other way too and is the reason #1267's decision survives intact: an
//!    audience-bound token is useless *against the API server*, so projecting one
//!    into a guest does not hand the guest a Kubernetes identity. It hands it a
//!    plant-broker identity and nothing else.
//! 2. **Namespace.** The username must be a ServiceAccount in the one namespace
//!    computers are vended into. Every other workload in athens is refused here
//!    rather than at the network layer alone, so the NetworkPolicy and this check
//!    have to *both* fail before an unexpected caller becomes a tenant.
//! 3. **Pod binding.** The tenant is the calling pod's name, which the API server
//!    reports in `extra` only for a token bound to a pod. A token that is not
//!    pod-bound — a legacy Secret-based one, or a namespace-wide mint — yields no
//!    per-computer name, so it is refused instead of collapsing every computer
//!    into one shared tenant. Tenancy is per-computer by construction, not by
//!    convention.
//!
//! ## Why a cache
//!
//! One reconcile hands over dozens of seals, and each is a separate request. A
//! TokenReview per object would put a synchronous API-server round trip in the
//! upload path of a service whose whole job is durability. Verdicts are cached
//! against the SHA-256 of the token — never the token itself — for the same 60s
//! the tailnet leg caches a whois. A revoked pod therefore keeps its name for at
//! most a minute, which is bounded by the same window the netmap already had.

use super::Tenant;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// In-cluster defaults. Every one of them is overridable so the leg can be
/// pointed at a kind cluster for proving without editing code.
pub const DEFAULT_API: &str = "https://kubernetes.default.svc";
pub const DEFAULT_CA: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
pub const DEFAULT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
/// The audience a caller's token must carry. Not `kubernetes.default.svc`: the
/// point is that this token opens this service and nothing else.
pub const DEFAULT_AUDIENCE: &str = "plant-broker";
/// The one namespace a tenant may be vended into (`pillars/computers`,
/// `internal/computers/spec.go`).
pub const DEFAULT_NAMESPACE: &str = "computers";

/// The API server's own name for the pod a bound token was projected into.
/// Stable Kubernetes API since 1.30; athens is on 1.35.
const POD_NAME_EXTRA: &str = "authentication.kubernetes.io/pod-name";

const CACHE_TTL: Duration = Duration::from_secs(60);

/// A caller's token is presented on every request; a runaway client must not be
/// able to grow this without bound between TTL sweeps.
const MAX_CACHED: usize = 512;

pub struct Config {
    pub api: String,
    pub ca: PathBuf,
    /// The broker's *own* token, used to authenticate the TokenReview call. Read
    /// per call rather than at startup: the kubelet rotates it in place, and a
    /// copy held from process start stops working after roughly an hour.
    pub token: PathBuf,
    pub audience: String,
    pub namespace: String,
}

impl Config {
    /// The leg as the deployment configures it, or `None` when this process is
    /// not running against a cluster it can ask. Absence is not an error — the
    /// caller may have another explicitly configured identity leg — but it is
    /// announced by the caller, because a broker that silently lost its ability
    /// to name computers would look healthy while every guest got a 403.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let ca = PathBuf::from(env("SEAL_BROKER_KUBE_CA").unwrap_or_else(|| DEFAULT_CA.into()));
        let token =
            PathBuf::from(env("SEAL_BROKER_KUBE_TOKEN").unwrap_or_else(|| DEFAULT_TOKEN.into()));
        if !ca.exists() || !token.exists() {
            return None;
        }
        Some(Config {
            api: env("SEAL_BROKER_KUBE_API").unwrap_or_else(|| DEFAULT_API.into()),
            ca,
            token,
            audience: env("SEAL_BROKER_TOKEN_AUDIENCE").unwrap_or_else(|| DEFAULT_AUDIENCE.into()),
            namespace: env("SEAL_BROKER_TENANT_NAMESPACE")
                .unwrap_or_else(|| DEFAULT_NAMESPACE.into()),
        })
    }
}

pub struct Identities {
    review_url: String,
    token: PathBuf,
    audience: String,
    namespace: String,
    client: reqwest::Client,
    cache: Mutex<HashMap<[u8; 32], (Tenant, Instant)>>,
}

impl Identities {
    pub fn new(config: Config) -> Result<Self> {
        let pem = std::fs::read(&config.ca)
            .with_context(|| format!("read the cluster CA at {}", config.ca.display()))?;
        let ca = reqwest::Certificate::from_pem(&pem)
            .with_context(|| format!("parse the cluster CA at {}", config.ca.display()))?;
        let client = reqwest::Client::builder()
            // Only the cluster's own CA, never the public roots: the API server
            // is the one host this client ever talks to, and a publicly-trusted
            // certificate for that name would be somebody else's.
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca)
            // A hung API server must not hold a seal upload open behind it.
            .timeout(Duration::from_secs(10))
            .build()
            .context("build the TokenReview client")?;
        Ok(Identities {
            review_url: format!(
                "{}/apis/authentication.k8s.io/v1/tokenreviews",
                config.api.trim_end_matches('/')
            ),
            token: config.token,
            audience: config.audience,
            namespace: config.namespace,
            client,
            cache: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(
        review_url: String,
        token: PathBuf,
        audience: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            review_url,
            token,
            audience: audience.into(),
            namespace: namespace.into(),
            client: reqwest::Client::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn audience(&self) -> &str {
        &self.audience
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The tenant a projected token names, or an error saying which check failed.
    pub async fn resolve(&self, token: &str) -> Result<Tenant> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some(hit) = self.cached(&digest) {
            return Ok(hit);
        }
        let review = self.review(token).await?;
        let tenant = self.tenant_of(&review)?;
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHED {
            let now = Instant::now();
            cache.retain(|_, (_, at)| now.duration_since(*at) < CACHE_TTL);
            // Still full of live entries: a genuine burst of distinct callers.
            // Serving them uncached is slower but correct; growing without a
            // bound is neither.
            if cache.len() < MAX_CACHED {
                cache.insert(digest, (tenant.clone(), Instant::now()));
            }
        } else {
            cache.insert(digest, (tenant.clone(), Instant::now()));
        }
        Ok(tenant)
    }

    fn cached(&self, digest: &[u8; 32]) -> Option<Tenant> {
        let mut cache = self.cache.lock().unwrap();
        match cache.get(digest) {
            Some((tenant, at)) if at.elapsed() < CACHE_TTL => Some(tenant.clone()),
            Some(_) => {
                cache.remove(digest);
                None
            }
            None => None,
        }
    }

    async fn review(&self, token: &str) -> Result<TokenReview> {
        let ours = read_token(&self.token)?;
        let response = self
            .client
            .post(&self.review_url)
            .bearer_auth(ours)
            .json(&serde_json::json!({
                "apiVersion": "authentication.k8s.io/v1",
                "kind": "TokenReview",
                "spec": { "token": token, "audiences": [self.audience] },
            }))
            .send()
            .await
            .context("submit a TokenReview to the API server")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read the TokenReview response")?;
        if !status.is_success() {
            // The broker's own RBAC failing looks identical to a bad caller from
            // the caller's side, so name the status here — this is the line that
            // distinguishes "your token is no good" from "we cannot ask".
            bail!(
                "the API server refused the TokenReview: {status} {}",
                body.trim()
            );
        }
        serde_json::from_str(&body).context("parse the TokenReview verdict")
    }

    fn tenant_of(&self, review: &TokenReview) -> Result<Tenant> {
        let status = &review.status;
        if !status.authenticated {
            let reason = status.error.as_deref().unwrap_or("no reason given");
            bail!("the API server did not authenticate this token: {reason}");
        }
        // The API server enforces the audience it was asked about; this re-reads
        // its answer so a future version that ever returned a wider set could not
        // widen this leg silently.
        if !status.audiences.iter().any(|a| a == &self.audience) {
            bail!(
                "the token is valid but not for audience {:?} (it carries {:?})",
                self.audience,
                status.audiences
            );
        }
        let expected = format!("system:serviceaccount:{}:", self.namespace);
        let Some(service_account) = status.user.username.strip_prefix(&expected) else {
            bail!(
                "{} is authenticated but is not a ServiceAccount in namespace {:?}, \
                 which is the only namespace this broker vends tenants from",
                status.user.username,
                self.namespace
            );
        };
        let Some(pod) = status
            .user
            .extra
            .get(POD_NAME_EXTRA)
            .and_then(|values| values.first())
            .map(String::as_str)
            .filter(|pod| !pod.trim().is_empty())
        else {
            bail!(
                "the token for {service_account} is not bound to a pod, so it names no \
                 single computer; tenancy here is per-computer and will not fall back \
                 to a shared name"
            );
        };
        Tenant::from_pod_name(pod).with_context(|| {
            format!("the API server named the pod {pod:?}, which yields no usable tenant")
        })
    }
}

fn read_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path).with_context(|| {
        format!(
            "read this broker's own ServiceAccount token at {}",
            path.display()
        )
    })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!(
            "this broker's ServiceAccount token at {} is empty",
            path.display()
        );
    }
    Ok(token)
}

#[derive(Deserialize)]
struct TokenReview {
    #[serde(default)]
    status: TokenReviewStatus,
}

#[derive(Deserialize, Default)]
struct TokenReviewStatus {
    #[serde(default)]
    authenticated: bool,
    #[serde(default)]
    audiences: Vec<String>,
    #[serde(default)]
    user: UserInfo,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct UserInfo {
    #[serde(default)]
    username: String,
    #[serde(default)]
    extra: HashMap<String, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `Identities` whose HTTP client is never used — every test here
    /// drives `tenant_of` against a verdict, which is where all three checks
    /// live. Reaching the network would be the bug.
    fn identities() -> Identities {
        Identities {
            review_url: "https://127.0.0.1:1/unused".into(),
            token: PathBuf::from("/nonexistent/token"),
            audience: DEFAULT_AUDIENCE.into(),
            namespace: DEFAULT_NAMESPACE.into(),
            client: reqwest::Client::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn verdict(json: serde_json::Value) -> TokenReview {
        serde_json::from_value(json).unwrap()
    }

    fn computer(pod: &str) -> serde_json::Value {
        serde_json::json!({
            "status": {
                "authenticated": true,
                "audiences": ["plant-broker"],
                "user": {
                    "username": "system:serviceaccount:computers:computer-agent-sandbox",
                    "extra": { POD_NAME_EXTRA: [pod] },
                },
            }
        })
    }

    #[test]
    fn a_pod_bound_token_names_the_computer_and_not_its_service_account() {
        let tenant = identities()
            .tenant_of(&verdict(computer("computer-cohnesor-scratch-0")))
            .unwrap();
        // The SA is shared by every agent-sandbox computer, so a tenant derived
        // from it would collapse them all into one. The pod is the computer.
        assert_eq!(tenant.as_str(), "computer-cohnesor-scratch-0");
    }

    #[test]
    fn an_unauthenticated_token_is_nobody() {
        let error = identities()
            .tenant_of(&verdict(serde_json::json!({
                "status": { "authenticated": false, "error": "invalid bearer token" }
            })))
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid bearer token"), "{error}");
    }

    #[test]
    fn a_token_for_another_audience_is_refused() {
        let mut v = computer("computer-cohnesor-scratch-0");
        v["status"]["audiences"] = serde_json::json!(["https://kubernetes.default.svc"]);
        let error = identities().tenant_of(&verdict(v)).unwrap_err().to_string();
        assert!(error.contains("not for audience"), "{error}");
    }

    #[test]
    fn a_service_account_from_another_namespace_is_refused() {
        let mut v = computer("some-pod-0");
        v["status"]["user"]["username"] =
            serde_json::json!("system:serviceaccount:kube-system:coredns");
        let error = identities().tenant_of(&verdict(v)).unwrap_err().to_string();
        assert!(error.contains("namespace"), "{error}");
        // A human's own identity is not a tenant either, whatever it can do.
        let mut v = computer("some-pod-0");
        v["status"]["user"]["username"] = serde_json::json!("dex:cohnesor@cb-sisco.com");
        assert!(identities().tenant_of(&verdict(v)).is_err());
    }

    #[test]
    fn a_token_that_is_not_bound_to_a_pod_is_refused_rather_than_shared() {
        let mut v = computer("ignored");
        v["status"]["user"]["extra"] = serde_json::json!({});
        let error = identities().tenant_of(&verdict(v)).unwrap_err().to_string();
        assert!(error.contains("not bound to a pod"), "{error}");
        // Present but blank is the same absence, not an empty tenant name.
        let mut v = computer("   ");
        v["status"]["user"]["extra"] = serde_json::json!({ POD_NAME_EXTRA: ["   "] });
        assert!(identities().tenant_of(&verdict(v)).is_err());
    }

    #[test]
    fn a_missing_ca_or_token_leaves_the_leg_unconfigured_rather_than_half_built() {
        let env = |key: &str| match key {
            "SEAL_BROKER_KUBE_CA" => Some("/nonexistent/ca.crt".to_string()),
            _ => None,
        };
        assert!(Config::from_env(env).is_none());
    }
}
