//! Certificate source and setup state for the network front door.
//! This module lives in control, not frontdoor, so listener crates can depend on
//! the source without a dependency edge back from the control plane.

use std::sync::{Arc, RwLock};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

/// Whether the front door can serve authenticated application traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsState {
    /// Only the authenticated certificate-setup surface is available.
    Setup,
    /// An ACME certificate has been installed; the bootstrap key cannot return.
    Serving,
}

/// Non-secret certificate status for CLI and status API consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateStatus {
    /// Front-door state.
    pub state: TlsState,
    /// CA profile name.
    pub ca: Option<String>,
    /// Names on the certificate request.
    pub names: Vec<String>,
    /// Expiry expressed as a Unix timestamp, when a certificate is installed.
    pub expires_at: Option<i64>,
    /// Last issuance/renewal failure, sanitized before entering this type.
    pub last_error: Option<String>,
    /// SHA-256 fingerprint of the bootstrap leaf certificate.
    pub bootstrap_fingerprint: Option<String>,
}

/// Fail-closed problem identifier for disallowed setup-state requests.
pub const TLS_SETUP_REQUIRED: &str = "/problems/tls-setup-required";

/// Reject application surfaces until ACME issuance succeeds.
///
/// # Errors
/// Returns the typed TLS setup problem when ACME has not issued a certificate.
pub fn require_serving(state: TlsState) -> Result<(), &'static str> {
    if state == TlsState::Serving {
        Ok(())
    } else {
        Err(TLS_SETUP_REQUIRED)
    }
}

/// Snapshot plus change notification shared by TCP, QUIC and TURN-TLS.
/// The snapshot never returns the key bytes, only an opaque `CertifiedKey`.
pub trait CertificateSource: Send + Sync {
    /// Obtain the key for a new TLS handshake.
    fn current(&self) -> Arc<CertifiedKey>;
    /// Subscribe to updates, including setup-to-serving transitions.
    fn subscribe(&self) -> watch::Receiver<Arc<CertifiedKey>>;
    /// Obtain a status snapshot without private key material.
    fn status(&self) -> CertificateStatus;
}

/// A bootstrap certificate and opaque private key held only for secure persistence.
pub struct BootstrapCertificate {
    /// Certified key for the setup-only listener.
    pub certificate: Arc<CertifiedKey>,
    /// Fingerprint of the bootstrap certificate for host-side comparison.
    pub fingerprint: String,
    key: Vec<u8>,
}

impl BootstrapCertificate {
    /// Persist the key with private permissions, never returning its bytes.
    ///
    /// # Errors
    /// Returns an error if the state file cannot be written securely.
    pub fn persist(&self, state_dir: &std::path::Path) -> std::io::Result<()> {
        persist_bootstrap_key(state_dir, &self.key)
    }
}

/// A fresh P-256 bootstrap certificate, restricted to the public host SAN.
/// Persist its key to a private 0600 state file; delete that file at ACME install.
///
/// # Errors
/// Returns an error if certificate generation or rustls key parsing fails.
pub fn bootstrap_certificate(
    host: &str,
) -> Result<BootstrapCertificate, Box<dyn std::error::Error + Send + Sync>> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = rcgen::CertificateParams::new(vec![host.to_owned()])?;
    let now = ::time::OffsetDateTime::now_utc();
    params.not_before = now - ::time::Duration::minutes(5);
    params.not_after = now + ::time::Duration::days(7);
    let der = params.self_signed(&key)?.der().to_vec();
    let fingerprint = Sha256::digest(&der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    let private = key.serialize_der();
    let signing = rustls::crypto::aws_lc_rs::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(private.clone()),
    ))?;
    Ok(BootstrapCertificate {
        certificate: Arc::new(CertifiedKey::new(vec![CertificateDer::from(der)], signing)),
        fingerprint,
        key: private,
    })
}

/// Persist a bootstrap key in a private directory; replace it on each boot.
/// Only call this with a directory controlled by the operator, never an untrusted path.
///
/// # Errors
/// Returns an error if the directory or file cannot be written securely.
fn persist_bootstrap_key(state_dir: &std::path::Path, key: &[u8]) -> std::io::Result<()> {
    use rustix::fs::{FileType, Mode, OFlags};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if state_dir
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::other("state_dir cannot contain '..'"));
    }
    crate::web_capture_startup::reject_symlink(state_dir)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)?;
    crate::web_capture_startup::reject_symlink(state_dir)?;
    let dir = rustix::fs::open(
        state_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&dir)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o700
    {
        return Err(std::io::Error::other(
            "state_dir must be an owner-private 0700 directory",
        ));
    }
    let mut temp = tempfile::NamedTempFile::new_in(state_dir)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    temp.write_all(key)?;
    temp.as_file().sync_all()?;
    temp.persist(state_dir.join("bootstrap.key"))
        .map_err(|e| e.error)?;
    std::fs::File::open(state_dir)?.sync_all()?;
    Ok(())
}

/// Build an RFC 8737 challenge certificate for one identifier and token digest.
/// The certificate is only usable with ALPN `acme-tls/1`, never as a site cert.
///
/// # Errors
/// Returns an error if the key or certificate cannot be generated.
pub fn alpn_challenge_certificate(
    host: &str,
    key_authorization: &[u8],
) -> Result<Arc<CertifiedKey>, Box<dyn std::error::Error + Send + Sync>> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = rcgen::CertificateParams::new(vec![host.to_owned()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .custom_extensions
        .push(rcgen::CustomExtension::new_acme_identifier(
            &Sha256::digest(key_authorization),
        ));
    let der = params.self_signed(&key)?.der().to_vec();
    let signing = rustls::crypto::aws_lc_rs::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(key.serialize_der()),
    ))?;
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(der)],
        signing,
    )))
}

/// Resolver that selects a challenge only when ALPN is exclusively `acme-tls/1`.
/// The control task installs and removes challenges; handshake selection is read-only.
pub struct ChallengeResolver {
    /// Normal site certificate source.
    pub source: Arc<dyn CertificateSource>,
    challenges: RwLock<std::collections::HashMap<String, Arc<CertifiedKey>>>,
}

impl ChallengeResolver {
    /// Construct the resolver with an ACME or static certificate source.
    #[must_use]
    pub fn new(source: Arc<dyn CertificateSource>) -> Self {
        Self {
            source,
            challenges: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Install one temporary challenge before acknowledging it to the CA.
    pub fn publish(&self, name: String, key: Arc<CertifiedKey>) {
        self.challenges
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name, key);
    }

    /// Remove a temporary challenge after validation, including failure paths.
    pub fn remove(&self, name: &str) {
        self.challenges
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }
}

impl std::fmt::Debug for ChallengeResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChallengeResolver([redacted])")
    }
}

impl ResolvesServerCert for ChallengeResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let Some(mut alpn) = hello.alpn() else {
            return Some(self.source.current());
        };
        if alpn.next() != Some(b"acme-tls/1".as_slice()) || alpn.next().is_some() {
            return Some(self.source.current());
        }
        let name = hello.server_name()?;
        self.challenges
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
    }
}

/// Shared hot-swappable ACME certificate source; all updates are off the data path.
pub struct AcmeCertificateSource {
    sender: watch::Sender<Arc<CertifiedKey>>,
    status: RwLock<CertificateStatus>,
}

impl AcmeCertificateSource {
    /// Start in setup with a fresh bootstrap key and its public fingerprint.
    #[must_use]
    pub fn new(
        bootstrap: Arc<CertifiedKey>,
        fingerprint: String,
        ca: String,
        names: Vec<String>,
    ) -> Self {
        let (sender, _) = watch::channel(bootstrap);
        Self {
            sender,
            status: RwLock::new(CertificateStatus {
                state: TlsState::Setup,
                ca: Some(ca),
                names,
                expires_at: None,
                last_error: None,
                bootstrap_fingerprint: Some(fingerprint),
            }),
        }
    }

    /// Rotate an expiring bootstrap certificate in setup without restarting outputs.
    /// The caller schedules this off the data path before the seven-day expiry.
    ///
    /// # Errors
    /// Returns a certificate or I/O error; the previous certificate remains active.
    pub fn rotate_bootstrap(
        &self,
        host: &str,
        state_dir: &std::path::Path,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut status = self
            .status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if status.state != TlsState::Setup {
            return Ok(false);
        }
        let fresh = bootstrap_certificate(host)?;
        fresh.persist(state_dir)?;
        self.sender.send_replace(fresh.certificate);
        status.bootstrap_fingerprint = Some(fresh.fingerprint);
        Ok(true)
    }

    /// Install a verified ACME chain without interrupting existing handshakes.
    /// Remove the bootstrap key on disk before announcing the serving state.
    ///
    /// # Errors
    /// Returns an I/O error and preserves the previous certificate if key removal fails.
    pub fn install(
        &self,
        key: Arc<CertifiedKey>,
        expires_at: i64,
        state_dir: &std::path::Path,
    ) -> std::io::Result<()> {
        let mut status = self
            .status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::fs::remove_file(state_dir.join("bootstrap.key")) {
            Ok(()) => std::fs::File::open(state_dir)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.sender.send_replace(key);
        status.state = TlsState::Serving;
        status.expires_at = Some(expires_at);
        status.last_error = None;
        status.bootstrap_fingerprint = None;
        Ok(())
    }

    /// Report a sanitized failure; the active certificate never changes.
    pub fn report_error(&self, reason: &'static str) {
        self.status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_error = Some(reason.to_owned());
    }
}

impl CertificateSource for AcmeCertificateSource {
    fn current(&self) -> Arc<CertifiedKey> {
        self.sender.borrow().clone()
    }
    fn subscribe(&self) -> watch::Receiver<Arc<CertifiedKey>> {
        self.sender.subscribe()
    }
    fn status(&self) -> CertificateStatus {
        self.status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Adapter for the existing `[control.tls] mode = "static"` path.
pub struct StaticCertificateSource {
    sender: watch::Sender<Arc<CertifiedKey>>,
}

impl StaticCertificateSource {
    /// Wrap an already parsed static certificate without changing static-mode behaviour.
    #[must_use]
    pub fn new(key: Arc<CertifiedKey>) -> Self {
        let (sender, _) = watch::channel(key);
        Self { sender }
    }
}

impl CertificateSource for StaticCertificateSource {
    fn current(&self) -> Arc<CertifiedKey> {
        self.sender.borrow().clone()
    }
    fn subscribe(&self) -> watch::Receiver<Arc<CertifiedKey>> {
        self.sender.subscribe()
    }
    fn status(&self) -> CertificateStatus {
        CertificateStatus {
            state: TlsState::Serving,
            ca: None,
            names: Vec::new(),
            expires_at: None,
            last_error: None,
            bootstrap_fingerprint: None,
        }
    }
}

/// A rustls resolver that reads the source on every new `ClientHello`.
pub struct SourceResolver(pub Arc<dyn CertificateSource>);

impl std::fmt::Debug for SourceResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceResolver([redacted])")
    }
}

impl ResolvesServerCert for SourceResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.current())
    }
}
