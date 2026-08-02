//! Local CA for the MITM leg of the forward proxy.
//!
//! Claude Code >= 2.1.196 disables Remote Control whenever `ANTHROPIC_BASE_URL`
//! names a host other than `api.anthropic.com`, so capture can no longer work by
//! pointing the client at Plant. It works by proxy instead: the client sets
//! `HTTPS_PROXY`, Plant answers `CONNECT`, and terminates TLS for the upstream
//! host with a leaf minted here. The client trusts `ca.pem` via
//! `NODE_EXTRA_CA_CERTS`.
//!
//! Both files persist. Regenerating the CA on every boot would invalidate the
//! copy every already-running client read at startup.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Directory holding `ca.pem` + `ca.key`. Shares the state dir with the job
/// ledger and crash log so there is one place to wipe.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("plant");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/state/plant")
}

pub struct Ca {
    issuer: Issuer<'static, KeyPair>,
    /// Where the CA cert is on disk — exactly what `NODE_EXTRA_CA_CERTS` and the
    /// NixOS wrapper's fail-closed guard point at.
    cert_path: PathBuf,
    /// System roots + this CA, for `SSL_CERT_FILE`. `NODE_EXTRA_CA_CERTS` alone
    /// is not enough: Claude Code checks Remote Control eligibility early,
    /// against the *system* trust store, before it applies the extra-certs
    /// path. That request is MITM'd like any other, so without this the check
    /// fails and reports the feature-flag service as unreachable — while every
    /// later request succeeds, because by then the extra CA is loaded.
    bundle_path: Option<PathBuf>,
    leaves: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
}

impl Ca {
    /// Load the persisted CA, or create it on first run.
    pub fn load_or_create() -> io::Result<Self> {
        Self::load_or_create_in(&state_dir())
    }

    pub fn load_or_create_in(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let cert_path = dir.join("ca.pem");
        let key_path = dir.join("ca.key");

        // Both halves or neither. A surviving key with a lost cert cannot be
        // rebuilt into the same issuer, and half a CA silently mints leaves no
        // client trusts.
        let existing = match (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path)) {
            (Ok(cert), Ok(key)) => Some((cert, key)),
            _ => None,
        };

        let (_cert_pem, key_pem) = match existing {
            Some(pair) => pair,
            None => {
                let pair = mint_ca().map_err(to_io)?;
                write_private(&key_path, &pair.1)?;
                std::fs::write(&cert_path, &pair.0)?;
                pair
            }
        };

        let key = KeyPair::from_pem(&key_pem).map_err(to_io)?;
        // Rebuilt from `ca_params()` rather than parsed back out of the PEM, which
        // would drag in x509-parser. Safe only because the same function produced
        // the stored cert: the subject DN and the key are what a leaf chains on,
        // and both are identical by construction. Keep it that way.
        let issuer = Issuer::new(ca_params().map_err(to_io)?, key);

        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let bundle_path = write_bundle(dir, &cert_pem)?;

        Ok(Self {
            issuer,
            cert_path,
            bundle_path,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// Merged system+local trust bundle, when a system bundle was locatable.
    pub fn bundle_path(&self) -> Option<&Path> {
        self.bundle_path.as_deref()
    }

    /// A `ServerConfig` presenting a leaf for `host`, minted on first use and
    /// cached. One host per allocator VM in practice, so an unbounded map is a
    /// handful of entries.
    pub fn server_config(&self, host: &str) -> io::Result<Arc<rustls::ServerConfig>> {
        if let Some(hit) = self.leaves.lock().unwrap().get(host) {
            return Ok(hit.clone());
        }
        let config = Arc::new(self.mint_leaf(host).map_err(to_io)?);
        self.leaves
            .lock()
            .unwrap()
            .insert(host.to_string(), config.clone());
        Ok(config)
    }

    fn mint_leaf(&self, host: &str) -> Result<rustls::ServerConfig, Box<dyn std::error::Error>> {
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, host);
            dn
        };
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.issuer)?;

        let chain = vec![CertificateDer::from(cert.der().to_vec())];
        let key_der = PrivateKeyDer::try_from(key.serialize_der())
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Name the provider rather than relying on a process default: reqwest and
        // tokio-tungstenite also pull rustls in, and `builder()` panics when the
        // default is ambiguous or uninstalled.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key_der)?;
        // The client speaks HTTP/1.1 to us; reqwest handles the upstream leg.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// Write `<dir>/ca-bundle.crt` = system roots + our CA, and return its path.
///
/// The system bundle comes from `PLANT_SYSTEM_CA_BUNDLE` (the NixOS unit passes
/// the exact `cacert` store path) or the usual Linux locations. Returns `None`
/// when none is found — macOS has no such file and does not need this — because
/// pointing `SSL_CERT_FILE` at a bundle holding only our CA would break TLS to
/// every host we do *not* intercept.
fn write_bundle(dir: &Path, ca_pem: &str) -> io::Result<Option<PathBuf>> {
    const CANDIDATES: [&str; 3] = [
        "/etc/ssl/certs/ca-bundle.crt",
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
    ];

    let system = std::env::var_os("PLANT_SYSTEM_CA_BUNDLE")
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|candidate| candidate.exists())
        });
    let Some(system) = system else {
        return Ok(None);
    };

    let mut merged = std::fs::read_to_string(&system)?;
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(ca_pem);

    let path = dir.join("ca-bundle.crt");
    std::fs::write(&path, merged)?;
    Ok(Some(path))
}

/// The one definition of the CA. Used both to self-sign the stored cert and to
/// rebuild the issuer on later boots — they must not diverge.
fn ca_params() -> Result<CertificateParams, rcgen::Error> {
    let mut params = CertificateParams::new(Vec::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "Plant local capture CA");
        dn.push(DnType::OrganizationName, "vaultr");
        dn
    };
    Ok(params)
}

fn mint_ca() -> Result<(String, String), Box<dyn std::error::Error>> {
    let key = KeyPair::generate()?;
    let cert = ca_params()?.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Write `0600` before any content lands — a private key must never exist
/// world-readable, not even for the window between create and chmod.
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

fn to_io(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plant-ca-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn pem(ca: &Ca) -> String {
        std::fs::read_to_string(ca.cert_path()).unwrap()
    }

    #[test]
    fn creates_then_reuses_the_same_ca() {
        let dir = tempdir("reuse");
        let first = Ca::load_or_create_in(&dir).unwrap();
        let before = pem(&first);
        let second = Ca::load_or_create_in(&dir).unwrap();
        assert_eq!(before, pem(&second));
        assert!(before.contains("BEGIN CERTIFICATE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir("perms");
        Ca::load_or_create_in(&dir).unwrap();
        let mode = std::fs::metadata(dir.join("ca.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "ca.key must be 0600, got {mode:o}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaf_is_cached_per_host() {
        let dir = tempdir("leaf");
        let ca = Ca::load_or_create_in(&dir).unwrap();
        let a = ca.server_config("api.anthropic.com").unwrap();
        let b = ca.server_config("api.anthropic.com").unwrap();
        let c = ca.server_config("claude.ai").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The merged bundle must contain the system roots as well as our CA.
    /// Shipping only our CA would make SSL_CERT_FILE reject every host we do
    /// not intercept, which looks like a network outage rather than a trust bug.
    #[test]
    fn bundle_merges_system_roots_with_our_ca() {
        let dir = tempdir("bundle");
        std::fs::create_dir_all(&dir).unwrap();
        let fake_system = dir.join("system-roots.pem");
        std::fs::write(&fake_system, "-----BEGIN CERTIFICATE-----\nSYSTEMROOT\n-----END CERTIFICATE-----").unwrap();
        std::env::set_var("PLANT_SYSTEM_CA_BUNDLE", &fake_system);

        let ca = Ca::load_or_create_in(&dir).unwrap();
        let bundle = std::fs::read_to_string(ca.bundle_path().expect("bundle written")).unwrap();

        assert!(bundle.contains("SYSTEMROOT"), "system roots must survive");
        assert!(bundle.contains(&pem(&ca)), "our CA must be appended");

        std::env::remove_var("PLANT_SYSTEM_CA_BUNDLE");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A half-written CA must be replaced, not loaded — a leaf signed by a key
    /// whose cert no client has is indistinguishable from a working proxy until
    /// the first TLS handshake fails.
    #[test]
    fn missing_key_regenerates_both_halves() {
        let dir = tempdir("halfca");
        let first = Ca::load_or_create_in(&dir).unwrap();
        let before = pem(&first);
        std::fs::remove_file(dir.join("ca.key")).unwrap();
        let second = Ca::load_or_create_in(&dir).unwrap();
        assert_ne!(before, pem(&second));
        std::fs::remove_dir_all(&dir).ok();
    }
}
