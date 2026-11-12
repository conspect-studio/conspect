//! The ACME issuance engine: one bounded attempt ([`AcmeEngine::issue_once`]),
//! crash cleanup, reload at start, renewal scheduling, expiry alarms, and the
//! service thread ([`AcmeService`]).
//!
//! The service runs on its OWN OS thread (`acme`) with its OWN current-thread
//! Tokio runtime: it never runs on, shares a lock with, or is awaited by the
//! output-clock path (invariants #1/#10). The only read path from the rest of
//! the process is `CertificateSource::{current, subscribe, status}`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, CertificateIdentifier, ChallengeType,
    ExternalAccountKey, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use multiview_config::{AcmeChallenge, CertificateAuthority, DnsProviderConfig, NetworkTlsConfig};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use tokio::sync::mpsc;

use super::cloudflare::SecretLookup;
use super::dns::{challenge_fqdn, DnsProvider, TxtHandle};
use super::egress::{
    AcmeHttp, EgressClient, EgressPolicy, EgressTimeouts, HttpFailure, ACME_BODY_CAP,
};
use super::store::{self, ActiveMeta, Ledger};
use super::AcmeError;
use crate::certificate_source::{
    alpn_challenge_certificate, AcmeCertificateSource, CertificateAlarm, ChallengeResolver,
};

/// Default deadline of one whole issuance attempt.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(600);
/// Deadline for each cleanup call (`remove_txt`).
const CLEANUP_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for the whole start-up ledger cleanup.
const CLEANUP_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Order polling deadline (ready, then certificate).
const POLL_TIMEOUT: Duration = Duration::from_secs(120);
/// First failure backoff.
const BACKOFF_BASE: Duration = Duration::from_secs(15 * 60);
/// Backoff cap.
const BACKOFF_CAP: Duration = Duration::from_secs(6 * 60 * 60);
/// No retry or recheck is ever scheduled sooner than this.
const MIN_DELAY: Duration = Duration::from_secs(60);
/// Fallback-renewal jitter cap.
const JITTER_CAP_S: i64 = 24 * 60 * 60;
/// Longest the service sleeps before re-evaluating its schedule.
const MAX_SLEEP: Duration = Duration::from_secs(6 * 60 * 60);

/// Hook invoked when the alarm level changes (the boot wiring raises/clears the
/// `tls-certificate` health warning from it).
pub type AlarmHook = Arc<dyn Fn(CertificateAlarm) + Send + Sync>;

/// Non-blocking "issue now" for the control plane (see [`AcmeHandle::trigger`]).
pub type IssueTrigger = Arc<dyn Fn() -> Result<(), SendError> + Send + Sync>;

/// A successfully issued and installed certificate (non-secret facts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// Leaf `notBefore`, Unix seconds.
    pub not_before: i64,
    /// Leaf `notAfter`, Unix seconds.
    pub not_after: i64,
    /// SAN set.
    pub names: Vec<String>,
    /// RFC 9773 identifier (`aki.serial`) when derivable.
    pub ari_cert_id: Option<String>,
}

impl From<&ActiveMeta> for Issued {
    fn from(meta: &ActiveMeta) -> Self {
        Self {
            not_before: meta.not_before,
            not_after: meta.not_after,
            names: meta.names.clone(),
            ari_cert_id: meta.ari_cert_id.clone(),
        }
    }
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn random_u64() -> u64 {
    uuid::Uuid::new_v4().as_u64_pair().0
}

/// A uniformly random instant in the ARI suggested window `[start, end]`.
/// The window is clipped to `now`; a window that has ended means "now".
/// `pick` is a uniformly random `u64`.
#[must_use]
pub fn ari_renewal_at(start: i64, end: i64, now: i64, pick: u64) -> i64 {
    if end <= now {
        return now;
    }
    let start = start.max(now);
    let span = u64::try_from(end.saturating_sub(start)).unwrap_or(0);
    let offset = i64::try_from(pick % span.saturating_add(1)).unwrap_or(0);
    start.saturating_add(offset)
}

/// The non-ARI renewal instant: two thirds into the lifetime, with a jitter of
/// ±(lifetime/30), capped at one day. `pick` is a uniformly random `u64`.
#[must_use]
pub fn fallback_renewal_at(not_before: i64, not_after: i64, pick: u64) -> i64 {
    let lifetime = not_after.saturating_sub(not_before).max(0);
    let base = not_before.saturating_add(lifetime.saturating_mul(2) / 3);
    let jitter = (lifetime / 30).min(JITTER_CAP_S);
    if jitter == 0 {
        return base;
    }
    let width = u64::try_from(jitter.saturating_mul(2))
        .unwrap_or(0)
        .saturating_add(1);
    let offset = i64::try_from(pick % width)
        .unwrap_or(0)
        .saturating_sub(jitter);
    base.saturating_add(offset)
}

/// Failure backoff after `failures` consecutive failures (≥ 1): 15 min
/// doubling, capped at 6 h, never shorter than 60 s.
#[must_use]
pub fn backoff_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    BACKOFF_BASE
        .checked_mul(factor)
        .unwrap_or(BACKOFF_CAP)
        .min(BACKOFF_CAP)
        .max(MIN_DELAY)
}

/// Delay until the next ARI check: the CA's `Retry-After` is a floor, and so
/// is 60 s; never more than 6 h.
#[must_use]
pub fn ari_recheck_delay(retry_after: Duration) -> Duration {
    retry_after.max(MIN_DELAY).min(BACKOFF_CAP)
}

/// The expiry alarm for a certificate valid `[not_before, not_after)` at `now`.
///
/// Past `not_after` → `Expired` (always). Otherwise only while renewal is
/// failing: strictly under 25 % of the lifetime left → `Critical`, strictly
/// under 50 % → `Warning`.
#[must_use]
pub fn alarm_for(
    not_before: i64,
    not_after: i64,
    now: i64,
    renewal_failing: bool,
) -> CertificateAlarm {
    if now >= not_after {
        return CertificateAlarm::Expired;
    }
    if !renewal_failing {
        return CertificateAlarm::None;
    }
    let total = i128::from(not_after.saturating_sub(not_before).max(1));
    let remaining = i128::from(not_after.saturating_sub(now));
    if remaining * 4 < total {
        CertificateAlarm::Critical
    } else if remaining * 2 < total {
        CertificateAlarm::Warning
    } else {
        CertificateAlarm::None
    }
}

/// Map an instant-acme error to a category. HTTP failures carry their own.
fn acme_category(error: &instant_acme::Error, default: AcmeError) -> AcmeError {
    match error {
        instant_acme::Error::Other(inner) => inner
            .downcast_ref::<HttpFailure>()
            .map_or(default, |failure| failure.0),
        instant_acme::Error::Api(problem) => {
            let kind = problem
                .r#type
                .as_deref()
                .and_then(|t| t.strip_prefix("urn:ietf:params:acme:error:"))
                .unwrap_or("");
            match kind {
                "externalAccountRequired" => AcmeError::Eab,
                "badNonce" | "rateLimited" | "serverInternal" | "badSignatureAlgorithm" => {
                    AcmeError::Authority
                }
                "incorrectResponse" | "dns" | "connection" | "tls" | "caa" | "unauthorized"
                | "rejectedIdentifier" => AcmeError::Challenge,
                _ => default,
            }
        }
        instant_acme::Error::Timeout(_) => AcmeError::Timeout,
        _ => default,
    }
}

/// The reverse-DNS SNI name RFC 8738 §6 uses for an IP identifier.
#[must_use]
pub fn reverse_name(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            format!("{d}.{c}.{b}.{a}.in-addr.arpa")
        }
        std::net::IpAddr::V6(v6) => {
            use std::fmt::Write as _;
            let mut out = String::with_capacity(72);
            for byte in v6.octets().iter().rev() {
                let _ = write!(out, "{:x}.{:x}.", byte & 0x0f, byte >> 4);
            }
            out.push_str("ip6.arpa");
            out
        }
    }
}

/// Parsed, verified certificate material.
struct Parsed {
    key: Arc<CertifiedKey>,
    not_before: i64,
    not_after: i64,
    names: Vec<String>,
    ari_cert_id: Option<String>,
}

fn parse_bundle(chain_pem: &[u8], key_pem: &[u8]) -> Result<Parsed, AcmeError> {
    let certs = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AcmeError::Mismatch)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|_| AcmeError::Mismatch)?;
    let signer = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key)
        .map_err(|_| AcmeError::Mismatch)?;
    let leaf = certs.first().ok_or(AcmeError::Mismatch)?;
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf).map_err(|_| AcmeError::Mismatch)?;
    let not_before = parsed.validity().not_before.timestamp();
    let not_after = parsed.validity().not_after.timestamp();
    if not_after <= not_before {
        return Err(AcmeError::Mismatch);
    }
    let mut names = Vec::new();
    if let Ok(Some(san)) = parsed.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::DNSName(dns) => {
                    names.push(dns.to_ascii_lowercase());
                }
                x509_parser::extensions::GeneralName::IPAddress(bytes) => {
                    let ip = match bytes.len() {
                        4 => <[u8; 4]>::try_from(*bytes).ok().map(std::net::IpAddr::from),
                        16 => <[u8; 16]>::try_from(*bytes)
                            .ok()
                            .map(std::net::IpAddr::from),
                        _ => None,
                    };
                    names.push(ip.ok_or(AcmeError::Mismatch)?.to_string());
                }
                _ => {}
            }
        }
    }
    let ari_cert_id = CertificateIdentifier::try_from(leaf)
        .ok()
        .map(|id| id.to_string());
    let certified = Arc::new(CertifiedKey::new(certs, signer));
    certified.keys_match().map_err(|_| AcmeError::Mismatch)?;
    Ok(Parsed {
        key: certified,
        not_before,
        not_after,
        names,
        ari_cert_id,
    })
}

const META_BEGIN: &str = "-----BEGIN CONSPECT ACME META-----";
const META_END: &str = "-----END CONSPECT ACME META-----";

/// The non-secret metadata as a PEM-style section appended to `active.pem`.
/// PEM readers skip a section with an unknown label, so the bundle stays a
/// plain key + chain for rustls and for `openssl`.
fn meta_section(meta: &ActiveMeta) -> Result<String, AcmeError> {
    use base64::Engine;
    let json = serde_json::to_vec(meta).map_err(|_| AcmeError::Storage)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(json);
    let mut out = format!("\n{META_BEGIN}\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out.push_str(META_END);
    out.push('\n');
    Ok(out)
}

/// Read the metadata section of a stored bundle, if present and well formed.
fn meta_from_bundle(bundle: &[u8]) -> Option<ActiveMeta> {
    use base64::Engine;
    let text = std::str::from_utf8(bundle).ok()?;
    let start = text.find(META_BEGIN)?.checked_add(META_BEGIN.len())?;
    let rest = text.get(start..)?;
    let body: String = rest
        .get(..rest.find(META_END)?)?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let json = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    serde_json::from_slice(&json).ok()
}

/// Read the stored certificate's non-secret facts without installing it
/// (offline `multiview tls status`). `Ok(None)` when no certificate is stored.
///
/// # Errors
/// [`AcmeError::Storage`] / [`AcmeError::Mismatch`] for insecure or malformed state.
pub fn stored_meta(state_dir: &Path) -> Result<Option<ActiveMeta>, AcmeError> {
    if !state_dir.join(store::ACTIVE_PEM).exists() {
        return Ok(None);
    }
    let Some(bundle) = store::read_private(state_dir, store::ACTIVE_PEM)? else {
        return Ok(None);
    };
    let bundle = zeroize::Zeroizing::new(bundle);
    let parsed = parse_bundle(&bundle, &bundle)?;
    let stored = meta_from_bundle(&bundle);
    Ok(Some(ActiveMeta {
        not_before: parsed.not_before,
        not_after: parsed.not_after,
        ari_cert_id: parsed.ari_cert_id,
        ca: stored.as_ref().map_or_else(String::new, |m| m.ca.clone()),
        directory_url: stored.map_or_else(String::new, |m| m.directory_url),
        names: parsed.names,
    }))
}

/// Parse a stored RFC 9773 `aki.serial` identifier.
#[must_use]
pub fn parse_cert_id(id: &str) -> Option<CertificateIdentifier<'static>> {
    let (aki, serial) = id.split_once('.')?;
    let ok = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !ok(aki) || !ok(serial) {
        return None;
    }
    Some(CertificateIdentifier {
        authority_key_identifier: std::borrow::Cow::Owned(aki.to_owned()),
        serial: std::borrow::Cow::Owned(serial.to_owned()),
    })
}

fn normalized(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .map(|n| match n.parse::<std::net::IpAddr>() {
            Ok(ip) => ip.to_string(),
            Err(_) => n.to_ascii_lowercase(),
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What one order published, so cleanup can remove it.
enum Published {
    Txt(TxtHandle),
    Alpn(String),
}

/// The ACME engine for one `[network.tls]` configuration.
pub struct AcmeEngine {
    config: NetworkTlsConfig,
    directory_url: String,
    dns: Option<Arc<dyn DnsProvider>>,
    resolver: Arc<ChallengeResolver>,
    source: Arc<AcmeCertificateSource>,
    policy: EgressPolicy,
    timeouts: EgressTimeouts,
    attempt_timeout: Duration,
    secrets: SecretLookup,
    alarm_hook: Option<AlarmHook>,
    alarm: Mutex<CertificateAlarm>,
    /// `(public host, period)`: rotate the bootstrap certificate this often while in setup.
    bootstrap_rotation: Option<(String, Duration)>,
}

impl std::fmt::Debug for AcmeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeEngine")
            .field("directory_url", &self.directory_url)
            .field("challenge", &self.config.challenge)
            .field("names", &self.config.domains)
            .finish_non_exhaustive()
    }
}

impl AcmeEngine {
    /// Build an engine. `dns` is required for DNS-01 and ignored otherwise.
    ///
    /// # Errors
    /// [`AcmeError::Config`] when the configuration is invalid or DNS-01 has no provider.
    pub fn new(
        config: NetworkTlsConfig,
        dns: Option<Arc<dyn DnsProvider>>,
        resolver: Arc<ChallengeResolver>,
        source: Arc<AcmeCertificateSource>,
    ) -> Result<Self, AcmeError> {
        config.validate().map_err(|_| AcmeError::Config)?;
        if config.challenge == AcmeChallenge::Dns01 && dns.is_none() {
            return Err(AcmeError::Config);
        }
        let directory_url = config
            .directory_url()
            .map_err(|_| AcmeError::Config)?
            .to_owned();
        let policy = EgressPolicy::directory(config.allow_private_directory);
        let secrets = SecretLookup::new(&config.state_dir);
        Ok(Self {
            config,
            directory_url,
            dns,
            resolver,
            source,
            policy,
            timeouts: EgressTimeouts::default(),
            attempt_timeout: ATTEMPT_TIMEOUT,
            secrets,
            alarm_hook: None,
            alarm: Mutex::new(CertificateAlarm::None),
            bootstrap_rotation: None,
        })
    }

    /// Rotate the 7-day bootstrap certificate every `every` while the source is
    /// in setup (on the service thread; the key is persisted 0600 in
    /// `state_dir`). The boot wiring passes six days.
    #[must_use]
    pub fn with_bootstrap_rotation(mut self, public_host: String, every: Duration) -> Self {
        self.bootstrap_rotation = Some((public_host, every.max(MIN_DELAY)));
        self
    }

    /// The configured directory URL.
    #[must_use]
    pub fn directory_url(&self) -> &str {
        &self.directory_url
    }

    /// Install a hook called on every alarm-level change.
    #[must_use]
    pub fn with_alarm_hook(mut self, hook: AlarmHook) -> Self {
        self.alarm_hook = Some(hook);
        self
    }

    /// Override the CA connection deadlines (tests shorten them).
    #[must_use]
    pub fn with_egress_timeouts(mut self, timeouts: EgressTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Override the whole-attempt deadline (default [`ATTEMPT_TIMEOUT`]).
    #[must_use]
    pub fn with_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.attempt_timeout = timeout;
        self
    }

    /// The certificate source this engine installs into.
    #[must_use]
    pub fn source(&self) -> &Arc<AcmeCertificateSource> {
        &self.source
    }

    /// The state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.config.state_dir
    }

    fn ledger(&self) -> Ledger {
        Ledger::new(self.config.state_dir.clone())
    }

    fn http(&self) -> Result<Box<dyn instant_acme::HttpClient>, AcmeError> {
        let client = EgressClient::new(
            self.policy.clone(),
            self.config.trust_anchor.as_deref(),
            self.timeouts,
            ACME_BODY_CAP,
        )?;
        Ok(Box::new(AcmeHttp::new(client)))
    }

    fn eab(&self) -> Result<Option<ExternalAccountKey>, AcmeError> {
        use base64::Engine as _;
        let (Some(kid_name), Some(hmac_name)) =
            (&self.config.eab_kid_env, &self.config.eab_hmac_env)
        else {
            return if self.config.ca == CertificateAuthority::Google {
                Err(AcmeError::Eab)
            } else {
                Ok(None)
            };
        };
        let kid = self
            .secrets
            .get(kid_name)
            .map_err(|_| AcmeError::Eab)?
            .ok_or(AcmeError::Eab)?;
        let hmac = self
            .secrets
            .get(hmac_name)
            .map_err(|_| AcmeError::Eab)?
            .ok_or(AcmeError::Eab)?;
        let trimmed = hmac.trim_end_matches('=');
        let bytes = zeroize::Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(trimmed)
                .map_err(|_| AcmeError::Eab)?,
        );
        if bytes.is_empty() {
            return Err(AcmeError::Eab);
        }
        Ok(Some(ExternalAccountKey::new(kid.to_string(), &bytes)))
    }

    /// Load the stored account for this directory, or register one.
    async fn account(&self) -> Result<Account, AcmeError> {
        let name = store::account_file_name(&self.directory_url)?;
        let dir = self.config.state_dir.clone();
        if let Some(raw) = store::read_private(&dir, &name)? {
            let raw = zeroize::Zeroizing::new(raw);
            let credentials: AccountCredentials =
                serde_json::from_slice(&raw).map_err(|_| AcmeError::Storage)?;
            return Account::builder_with_http(self.http()?)
                .from_credentials(credentials)
                .await
                .map_err(|e| acme_category(&e, AcmeError::Authority));
        }
        let eab = self.eab()?;
        let contact = self
            .config
            .contact_email
            .as_ref()
            .map(|email| format!("mailto:{email}"));
        let contacts: Vec<&str> = contact.as_deref().into_iter().collect();
        let registration = NewAccount {
            contact: &contacts,
            terms_of_service_agreed: true,
            only_return_existing: false,
        };
        let (account, credentials) = Account::builder_with_http(self.http()?)
            .create(&registration, self.directory_url.clone(), eab.as_ref())
            .await
            .map_err(|e| acme_category(&e, AcmeError::Authority))?;
        let raw = zeroize::Zeroizing::new(
            serde_json::to_vec(&credentials).map_err(|_| AcmeError::Storage)?,
        );
        store::write_private(&dir, &name, &raw)?;
        Ok(account)
    }

    /// Remove every TXT left in the ledger by a crashed or killed run (best
    /// effort, bounded). Entries whose removal succeeded are cleared.
    pub async fn cleanup_pending(&self) {
        let ledger = self.ledger();
        let entries = match ledger.load() {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    reason = error.as_reason(),
                    "acme: pending-TXT ledger unreadable"
                );
                return;
            }
        };
        if entries.is_empty() {
            return;
        }
        let work = async {
            for entry in entries {
                let Some(dns) = self.dns.as_ref().filter(|d| d.name() == entry.provider) else {
                    continue;
                };
                let Some(handle) = entry.handle else {
                    // Publish never returned a handle: nothing addressable to remove.
                    let _ = ledger.forget_intent(&entry.fqdn);
                    continue;
                };
                match tokio::time::timeout(CLEANUP_CALL_TIMEOUT, dns.remove_txt(&handle)).await {
                    Ok(Ok(())) => {
                        let _ = ledger.forget_handle(&handle);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(reason = error.as_reason(), "acme: stale TXT not removed");
                    }
                    Err(_) => tracing::warn!("acme: stale TXT removal timed out"),
                }
            }
        };
        if tokio::time::timeout(CLEANUP_TOTAL_TIMEOUT, work)
            .await
            .is_err()
        {
            tracing::warn!("acme: pending-TXT cleanup timed out");
        }
    }

    /// Install a stored certificate at start without contacting the CA.
    /// An expired certificate is installed too (it keeps serving; status
    /// reports `expired`). `Ok(None)` when none is stored.
    ///
    /// Blocking (file I/O + fsync): call from the ACME thread or `spawn_blocking`.
    ///
    /// # Errors
    /// [`AcmeError::Storage`] / [`AcmeError::Mismatch`] for insecure or malformed state.
    pub fn load_existing(&self) -> Result<Option<ActiveMeta>, AcmeError> {
        let dir = &self.config.state_dir;
        let Some(bundle) = store::read_private(dir, store::ACTIVE_PEM)? else {
            return Ok(None);
        };
        let bundle = zeroize::Zeroizing::new(bundle);
        let parsed = parse_bundle(&bundle, &bundle)?;
        // The metadata travels inside `active.pem`, so it can never disagree
        // with the certificate. A bundle without it (lost or unreadable) is
        // treated as issued by the configured directory: a still-valid
        // certificate is never thrown away for missing bookkeeping; the
        // normal renewal schedule replaces it in time.
        let stored = meta_from_bundle(&bundle);
        let meta = ActiveMeta {
            not_before: parsed.not_before,
            not_after: parsed.not_after,
            ari_cert_id: parsed.ari_cert_id.clone(),
            ca: stored
                .as_ref()
                .map_or_else(|| self.ca_name(), |m| m.ca.clone()),
            directory_url: stored
                .as_ref()
                .map_or_else(|| self.directory_url.clone(), |m| m.directory_url.clone()),
            names: parsed.names.clone(),
        };
        self.source
            .install(parsed.key, parsed.not_after, dir)
            .map_err(|_| AcmeError::Storage)?;
        self.update_alarm(alarm_for(
            meta.not_before,
            meta.not_after,
            unix_now(),
            false,
        ));
        Ok(Some(meta))
    }

    fn ca_name(&self) -> String {
        format!("{:?}", self.config.ca).to_ascii_lowercase()
    }

    /// Whether a stored certificate must be replaced regardless of schedule:
    /// different directory or a different name set than configured.
    #[must_use]
    pub fn needs_reissue(&self, meta: &ActiveMeta) -> bool {
        meta.directory_url != self.directory_url
            || normalized(&meta.names) != normalized(&self.config.domains)
    }

    /// One bounded issuance attempt. On success the certificate is persisted
    /// and installed; on any failure the current certificate stays. Every
    /// published TXT record and ALPN challenge is removed on every path,
    /// including the internal timeout.
    ///
    /// # Errors
    /// A credential-safe [`AcmeError`] category.
    pub async fn issue_once(
        &self,
        replaces: Option<CertificateIdentifier<'static>>,
    ) -> Result<Issued, AcmeError> {
        let mut published = Vec::new();
        let result =
            tokio::time::timeout(self.attempt_timeout, self.attempt(replaces, &mut published))
                .await
                .unwrap_or(Err(AcmeError::Timeout));
        self.cleanup(published).await;
        result
    }

    async fn cleanup(&self, published: Vec<Published>) {
        let ledger = self.ledger();
        for item in published {
            match item {
                Published::Alpn(name) => self.resolver.remove(&name),
                Published::Txt(handle) => {
                    let Some(dns) = &self.dns else { continue };
                    match tokio::time::timeout(CLEANUP_CALL_TIMEOUT, dns.remove_txt(&handle)).await
                    {
                        Ok(Ok(())) => {
                            let _ = ledger.forget_handle(&handle);
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(
                                reason = error.as_reason(),
                                "acme: TXT not removed; kept in ledger"
                            );
                        }
                        Err(_) => tracing::warn!("acme: TXT removal timed out; kept in ledger"),
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn attempt(
        &self,
        replaces: Option<CertificateIdentifier<'static>>,
        published: &mut Vec<Published>,
    ) -> Result<Issued, AcmeError> {
        let dns = match self.config.challenge {
            AcmeChallenge::Dns01 => {
                let dns = self.dns.as_ref().ok_or(AcmeError::Config)?;
                dns.validate_credential(&self.config.domains).await?;
                Some(dns)
            }
            AcmeChallenge::TlsAlpn01 => None,
        };
        let account = self.account().await?;
        let identifiers: Vec<Identifier> = self
            .config
            .domains
            .iter()
            .map(|name| {
                name.parse()
                    .map_or_else(|_| Identifier::Dns(name.clone()), Identifier::Ip)
            })
            .collect();
        let profile = self
            .config
            .acme_profile
            .as_deref()
            .filter(|wanted| account.profiles().any(|p| p.name == *wanted));
        let build = |with_replaces: bool| {
            let mut request = NewOrder::new(&identifiers);
            if let Some(profile) = profile {
                request = request.profile(profile);
            }
            if with_replaces {
                if let Some(id) = &replaces {
                    request = request.replaces(id.clone());
                }
            }
            request
        };
        let mut order = match account.new_order(&build(true)).await {
            // The directory does not offer ARI: order without `replaces`.
            Err(instant_acme::Error::Unsupported(_)) if replaces.is_some() => account
                .new_order(&build(false))
                .await
                .map_err(|e| acme_category(&e, AcmeError::Authority))?,
            other => other.map_err(|e| acme_category(&e, AcmeError::Authority))?,
        };
        let wanted = match self.config.challenge {
            AcmeChallenge::Dns01 => ChallengeType::Dns01,
            AcmeChallenge::TlsAlpn01 => ChallengeType::TlsAlpn01,
        };
        let propagation = match &self.config.dns {
            Some(DnsProviderConfig::Cloudflare {
                propagation_timeout_s,
                ..
            }) => Duration::from_secs(*propagation_timeout_s),
            None => Duration::from_secs(120),
        };
        let ledger = self.ledger();
        let mut authorizations = order.authorizations();
        while let Some(next) = authorizations.next().await {
            let mut authz = next.map_err(|e| acme_category(&e, AcmeError::Authority))?;
            match authz.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                _ => return Err(AcmeError::Challenge),
            }
            let mut challenge = authz
                .challenge(wanted.clone())
                .ok_or(AcmeError::Unsupported)?;
            let identifier = challenge.identifier().identifier.clone();
            let key_authorization = challenge.key_authorization();
            match (&identifier, dns) {
                (Identifier::Dns(name), Some(dns)) => {
                    let fqdn = challenge_fqdn(name);
                    let value = key_authorization.dns_value();
                    ledger.record_intent(dns.name(), &fqdn)?;
                    let handle = match dns.publish_txt(&fqdn, &value).await {
                        Ok(handle) => handle,
                        Err(error) => {
                            let _ = ledger.forget_intent(&fqdn);
                            return Err(error);
                        }
                    };
                    published.push(Published::Txt(handle.clone()));
                    ledger.record_handle(&fqdn, &handle)?;
                    dns.confirm_at_authoritative(&fqdn, &value, propagation)
                        .await?;
                }
                (Identifier::Dns(_) | Identifier::Ip(_), None) => {
                    let (sni, cert_name) = match &identifier {
                        Identifier::Ip(ip) => (reverse_name(*ip), ip.to_string()),
                        Identifier::Dns(name) => (name.clone(), name.clone()),
                        _ => return Err(AcmeError::Unsupported),
                    };
                    let certificate = alpn_challenge_certificate(
                        &cert_name,
                        key_authorization.as_str().as_bytes(),
                    )
                    .map_err(|_| AcmeError::Challenge)?;
                    self.resolver.publish(sni.clone(), certificate);
                    published.push(Published::Alpn(sni));
                }
                _ => return Err(AcmeError::Unsupported),
            }
            challenge
                .set_ready()
                .await
                .map_err(|e| acme_category(&e, AcmeError::Challenge))?;
        }
        let retry = RetryPolicy::new()
            .initial_delay(Duration::from_millis(500))
            .backoff(1.5)
            .timeout(POLL_TIMEOUT);
        let status = order
            .poll_ready(&retry)
            .await
            .map_err(|e| acme_category(&e, AcmeError::Challenge))?;
        if status != OrderStatus::Ready {
            return Err(AcmeError::Challenge);
        }
        let key_pem = zeroize::Zeroizing::new(
            order
                .finalize()
                .await
                .map_err(|e| acme_category(&e, AcmeError::Authority))?,
        );
        let chain_pem = order
            .poll_certificate(&retry)
            .await
            .map_err(|e| acme_category(&e, AcmeError::Authority))?;
        let parsed = parse_bundle(chain_pem.as_bytes(), key_pem.as_bytes())?;
        if normalized(&parsed.names) != normalized(&self.config.domains) {
            return Err(AcmeError::Mismatch);
        }
        let dir = self.config.state_dir.clone();
        let bundle = zeroize::Zeroizing::new(format!("{}\n{chain_pem}", key_pem.as_str()));
        let meta = ActiveMeta {
            not_before: parsed.not_before,
            not_after: parsed.not_after,
            ari_cert_id: parsed.ari_cert_id.clone(),
            ca: self.ca_name(),
            directory_url: self.directory_url.clone(),
            names: parsed.names.clone(),
        };
        // One file, one rename: key, chain and metadata commit together, and
        // the served key changes only after that commit (crash-safe).
        let bundle =
            zeroize::Zeroizing::new(format!("{}{}", bundle.as_str(), meta_section(&meta)?));
        let source = Arc::clone(&self.source);
        let key = Arc::clone(&parsed.key);
        let not_after = parsed.not_after;
        tokio::task::spawn_blocking(move || -> Result<(), AcmeError> {
            store::write_private(&dir, store::ACTIVE_PEM, bundle.as_bytes())?;
            source
                .install(key, not_after, &dir)
                .map_err(|_| AcmeError::Storage)
        })
        .await
        .map_err(|_| AcmeError::Storage)??;
        self.update_alarm(CertificateAlarm::None);
        Ok(Issued {
            not_before: parsed.not_before,
            not_after: parsed.not_after,
            names: parsed.names,
            ari_cert_id: parsed.ari_cert_id,
        })
    }

    /// When the certificate `meta` should be renewed: the ARI window when the
    /// directory offers it (with the delay until the next ARI check), else
    /// the lifetime fallback.
    pub async fn renewal_at(&self, meta: &ActiveMeta, now: i64) -> (i64, Option<Duration>) {
        if let Some(id) = &meta.ari_cert_id {
            if let Some(id) = parse_cert_id(id) {
                let ari = async {
                    let account = self.account().await.ok()?;
                    account.renewal_info(&id).await.ok()
                };
                if let Ok(Some((info, retry_after))) =
                    tokio::time::timeout(self.timeouts.request.saturating_mul(2), ari).await
                {
                    let at = ari_renewal_at(
                        info.suggested_window.start.unix_timestamp(),
                        info.suggested_window.end.unix_timestamp(),
                        now,
                        random_u64(),
                    );
                    return (at, Some(ari_recheck_delay(retry_after)));
                }
            }
        }
        (
            fallback_renewal_at(meta.not_before, meta.not_after, random_u64()),
            None,
        )
    }

    /// Publish an alarm level: logs and hooks fire only on change.
    pub fn update_alarm(&self, alarm: CertificateAlarm) {
        let mut current = self
            .alarm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == alarm {
            return;
        }
        *current = alarm;
        drop(current);
        self.source.set_alarm(alarm);
        match alarm {
            CertificateAlarm::None => tracing::info!("acme: certificate alarm cleared"),
            CertificateAlarm::Warning => {
                tracing::warn!(
                    "acme: renewal failing and under 50% of the certificate lifetime remains"
                );
            }
            CertificateAlarm::Critical => {
                tracing::error!(
                    "acme: renewal failing and under 25% of the certificate lifetime remains"
                );
            }
            CertificateAlarm::Expired => {
                tracing::error!("acme: the served certificate has expired");
            }
        }
        if let Some(hook) = &self.alarm_hook {
            hook(alarm);
        }
    }
}

/// A command to the ACME service thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcmeCommand {
    /// Run an issuance attempt now (ignored while one is running).
    IssueNow,
    /// Stop the service.
    Shutdown,
}

/// Why a command was not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The command queue is full; the command was dropped.
    #[error("ACME service busy")]
    Busy,
    /// The service has stopped.
    #[error("ACME service stopped")]
    Closed,
}

/// Handle to the running service. Dropping it stops the thread (bounded join).
#[derive(Debug)]
pub struct AcmeHandle {
    sender: Option<mpsc::Sender<AcmeCommand>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AcmeHandle {
    /// Queue a command without waiting (capacity 4; full = dropped).
    ///
    /// # Errors
    /// [`SendError::Busy`] when the queue is full, [`SendError::Closed`] after stop.
    pub fn send(&self, command: AcmeCommand) -> Result<(), SendError> {
        let sender = self.sender.as_ref().ok_or(SendError::Closed)?;
        sender.try_send(command).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Busy,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }

    /// A cloneable "issue now" trigger for the control plane. It holds only a
    /// weak sender, so it never keeps the service alive after this handle drops.
    #[must_use]
    pub fn trigger(&self) -> IssueTrigger {
        let weak = self.sender.as_ref().map(mpsc::Sender::downgrade);
        Arc::new(move || {
            let sender = weak
                .as_ref()
                .and_then(mpsc::WeakSender::upgrade)
                .ok_or(SendError::Closed)?;
            sender.try_send(AcmeCommand::IssueNow).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => SendError::Busy,
                mpsc::error::TrySendError::Closed(_) => SendError::Closed,
            })
        })
    }

    /// Whether the service thread has exited.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for AcmeHandle {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(AcmeCommand::Shutdown);
        }
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            tracing::warn!("acme: service thread did not stop within 2 s; detached");
        }
    }
}

/// The ACME service: its own OS thread and current-thread runtime.
#[derive(Debug)]
pub struct AcmeService;

impl AcmeService {
    /// Start the service thread for `engine`.
    ///
    /// # Errors
    /// An I/O error if the thread cannot be spawned.
    pub fn spawn(engine: AcmeEngine) -> std::io::Result<AcmeHandle> {
        let (sender, receiver) = mpsc::channel(4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let thread = std::thread::Builder::new()
            .name("acme".to_owned())
            .spawn(move || {
                runtime.block_on(run(engine, receiver));
                runtime.shutdown_timeout(Duration::from_secs(1));
            })?;
        Ok(AcmeHandle {
            sender: Some(sender),
            thread: Some(thread),
        })
    }
}

fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// Await `future` unless a `Shutdown` (or channel close) arrives first.
/// `IssueNow` during the future is ignored. `None` = shut down.
async fn unless_shutdown<F: std::future::Future>(
    future: F,
    receiver: &mut mpsc::Receiver<AcmeCommand>,
) -> Option<F::Output> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return Some(output),
            command = receiver.recv() => match command {
                Some(AcmeCommand::IssueNow) => {}
                Some(AcmeCommand::Shutdown) | None => return None,
            },
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run(engine: AcmeEngine, mut receiver: mpsc::Receiver<AcmeCommand>) {
    if unless_shutdown(engine.cleanup_pending(), &mut receiver)
        .await
        .is_none()
    {
        return;
    }
    let mut current = match engine.load_existing() {
        Ok(meta) => meta,
        Err(error) => {
            engine.source.report_error(error.as_reason());
            None
        }
    };
    let mut failures: u32 = 0;
    let mut next_attempt: Option<i64> = None;
    // Cached schedule for `current`: (renew_at, ARI recheck instant). `None`
    // recheck = lifetime fallback, fixed until the certificate changes.
    let mut ari: Option<(i64, Option<i64>)> = None;
    let mut force = false;
    let mut next_rotation = engine
        .bootstrap_rotation
        .as_ref()
        .map(|(_, every)| std::time::Instant::now() + *every);
    loop {
        if let (Some((host, every)), Some(at)) = (&engine.bootstrap_rotation, next_rotation) {
            if std::time::Instant::now() >= at {
                match engine.source.rotate_bootstrap(host, &engine.config.state_dir) {
                    Ok(true) => tracing::info!(
                        fingerprint = crate::certificate_source::CertificateSource::status(
                            engine.source.as_ref()
                        )
                        .bootstrap_fingerprint
                            .as_deref()
                            .unwrap_or(""),
                        "acme: bootstrap certificate rotated (compare this SHA-256 fingerprint with your browser's)"
                    ),
                    Ok(false) => {}
                    Err(_) => tracing::warn!("acme: bootstrap certificate rotation failed; the current one stays"),
                }
                next_rotation = Some(std::time::Instant::now() + *every);
            }
        }
        let now = unix_now();
        let due_at = match &current {
            None => now,
            Some(meta) if engine.needs_reissue(meta) => now,
            Some(meta) => match ari {
                Some((at, None)) => at,
                Some((at, Some(recheck))) if now < recheck => at,
                _ => {
                    let Some((at, recheck)) =
                        unless_shutdown(engine.renewal_at(meta, now), &mut receiver).await
                    else {
                        return;
                    };
                    ari = Some((at, recheck.map(|delay| now.saturating_add(secs(delay)))));
                    at
                }
            },
        };
        let wake = if force {
            now
        } else {
            next_attempt.map_or(due_at, |retry| retry.max(due_at))
        };
        engine
            .source
            .set_schedule(current.as_ref().map(|_| due_at), next_attempt);
        if let Some(meta) = &current {
            engine.update_alarm(alarm_for(
                meta.not_before,
                meta.not_after,
                now,
                failures > 0,
            ));
        }
        if now >= wake {
            force = false;
            let replaces = current
                .as_ref()
                .and_then(|m| m.ari_cert_id.clone())
                .and_then(|id| parse_cert_id(&id));
            let Some(result) = unless_shutdown(engine.issue_once(replaces), &mut receiver).await
            else {
                return;
            };
            match result {
                Ok(issued) => {
                    tracing::info!(not_after = issued.not_after, "acme: certificate installed");
                    current = Some(ActiveMeta {
                        not_before: issued.not_before,
                        not_after: issued.not_after,
                        ari_cert_id: issued.ari_cert_id,
                        ca: engine.ca_name(),
                        directory_url: engine.directory_url.clone(),
                        names: issued.names,
                    });
                    failures = 0;
                    next_attempt = None;
                    ari = None;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let delay = backoff_delay(failures);
                    next_attempt = Some(unix_now().saturating_add(secs(delay)));
                    engine.source.report_error(error.as_reason());
                    tracing::warn!(
                        reason = error.as_reason(),
                        failures,
                        "acme: issuance attempt failed"
                    );
                }
            }
            continue;
        }
        let mut sleep_for =
            Duration::from_secs(u64::try_from(wake.saturating_sub(now)).unwrap_or(0));
        if let Some((_, Some(recheck))) = ari {
            sleep_for = sleep_for.min(Duration::from_secs(
                u64::try_from(recheck.saturating_sub(now)).unwrap_or(0),
            ));
        }
        if let Some(at) = next_rotation {
            sleep_for = sleep_for.min(at.saturating_duration_since(std::time::Instant::now()));
        }
        let sleep_for = sleep_for.clamp(Duration::from_secs(1), MAX_SLEEP);
        tokio::select! {
            () = tokio::time::sleep(sleep_for) => {}
            command = receiver.recv() => match command {
                Some(AcmeCommand::IssueNow) => force = true,
                Some(AcmeCommand::Shutdown) | None => return,
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bundle_metadata_round_trips_and_pem_readers_ignore_it() {
        let meta = ActiveMeta {
            not_before: 1,
            not_after: 2,
            ari_cert_id: Some("a.b".to_owned()),
            ca: "custom".to_owned(),
            directory_url: "https://acme.example/dir".to_owned(),
            names: vec!["a.example".to_owned()],
        };
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["a.example".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let bundle = format!(
            "{}\n{}{}",
            key.serialize_pem(),
            cert.pem(),
            meta_section(&meta).unwrap()
        );
        assert_eq!(meta_from_bundle(bundle.as_bytes()), Some(meta));
        let parsed = parse_bundle(bundle.as_bytes(), bundle.as_bytes()).unwrap();
        assert_eq!(parsed.names, vec!["a.example".to_owned()]);
        // A bundle without the section (older layout, or the section lost)
        // still parses; the metadata is simply absent.
        let bare = format!("{}\n{}", key.serialize_pem(), cert.pem());
        assert!(meta_from_bundle(bare.as_bytes()).is_none());
        assert!(parse_bundle(bare.as_bytes(), bare.as_bytes()).is_ok());
    }

    /// Review #331 L-1 (media lens): a valid stored certificate whose metadata
    /// is missing must not be treated as foreign and re-issued at once.
    #[test]
    fn a_valid_bundle_without_metadata_is_kept_not_reissued() {
        use crate::certificate_source::CertificateSource as _;
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = NetworkTlsConfig {
            ca: CertificateAuthority::Letsencrypt,
            staging: true,
            challenge: AcmeChallenge::TlsAlpn01,
            domains: vec!["a.example.org".to_owned()],
            contact_email: None,
            acme_profile: None,
            directory: None,
            trust_anchor: None,
            allow_private_directory: false,
            eab_kid_env: None,
            eab_hmac_env: None,
            state_dir: state_dir.clone(),
            dns: None,
        };
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["a.example.org".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let bare = format!("{}\n{}", key.serialize_pem(), cert.pem());
        store::write_private(&state_dir, store::ACTIVE_PEM, bare.as_bytes()).unwrap();
        let boot = crate::certificate_source::bootstrap_certificate("a.example.org").unwrap();
        let source = Arc::new(AcmeCertificateSource::new(
            boot.certificate.clone(),
            boot.fingerprint,
            "letsencrypt".to_owned(),
            vec!["a.example.org".to_owned()],
        ));
        let resolver = Arc::new(ChallengeResolver::new(source.clone()));
        let engine = AcmeEngine::new(config, None, resolver, source.clone()).unwrap();
        let meta = engine.load_existing().unwrap().unwrap();
        assert!(!engine.needs_reissue(&meta), "{meta:?}");
        assert_eq!(meta.ca, "letsencrypt");
        assert!(!Arc::ptr_eq(&boot.certificate, &source.current()));
    }

    #[test]
    fn ari_picks_inside_the_window_or_now() {
        assert_eq!(ari_renewal_at(100, 200, 50, 0), 100);
        assert_eq!(ari_renewal_at(100, 200, 50, 100), 200);
        assert_eq!(ari_renewal_at(100, 200, 50, 101), 100);
        for pick in [0, 7, 99, u64::MAX] {
            let at = ari_renewal_at(100, 200, 150, pick);
            assert!((150..=200).contains(&at), "{at}");
        }
        assert_eq!(ari_renewal_at(100, 200, 250, 5), 250);
        assert_eq!(ari_renewal_at(100, 200, 200, 5), 200);
    }

    #[test]
    fn fallback_is_two_thirds_with_bounded_jitter() {
        let day = 86_400;
        let nb = 1_000_000;
        let na = nb + 90 * day;
        let base = nb + 60 * day;
        // 90 d / 30 = 3 d of jitter, capped to 1 d.
        for pick in [0, 1, 12_345, u64::MAX] {
            let at = fallback_renewal_at(nb, na, pick);
            assert!((base - day..=base + day).contains(&at), "{at}");
        }
        // Short lifetime: 6 days → ±(6 d / 30) = ±4.8 h.
        let na = nb + 6 * day;
        let base = nb + 4 * day;
        for pick in [0, 999, u64::MAX] {
            let at = fallback_renewal_at(nb, na, pick);
            assert!((base - 17_280..=base + 17_280).contains(&at), "{at}");
        }
        assert_eq!(
            fallback_renewal_at(nb, nb + 2 * day, 0),
            nb + (2 * day * 2) / 3 - (2 * day / 30)
        );
        assert_eq!(fallback_renewal_at(10, 10, 5), 10);
    }

    #[test]
    fn backoff_doubles_from_15_minutes_to_a_6_hour_cap() {
        let m = 60;
        let expected = [15 * m, 30 * m, 60 * m, 120 * m, 240 * m, 360 * m, 360 * m];
        for (i, want) in expected.iter().enumerate() {
            let failures = u32::try_from(i + 1).unwrap();
            assert_eq!(backoff_delay(failures).as_secs(), *want, "{failures}");
        }
        assert_eq!(backoff_delay(u32::MAX).as_secs(), 360 * m);
        assert!(backoff_delay(0) >= Duration::from_secs(60));
        assert_eq!(ari_recheck_delay(Duration::ZERO), Duration::from_secs(60));
        assert_eq!(
            ari_recheck_delay(Duration::from_secs(3600)),
            Duration::from_secs(3600)
        );
        assert_eq!(ari_recheck_delay(Duration::from_secs(86_400)), BACKOFF_CAP);
    }

    #[test]
    fn alarm_thresholds_at_50_and_25_percent() {
        let (nb, na) = (0, 1000);
        assert_eq!(alarm_for(nb, na, 900, false), CertificateAlarm::None);
        assert_eq!(alarm_for(nb, na, 499, true), CertificateAlarm::None);
        assert_eq!(alarm_for(nb, na, 500, true), CertificateAlarm::None);
        assert_eq!(alarm_for(nb, na, 501, true), CertificateAlarm::Warning);
        assert_eq!(alarm_for(nb, na, 750, true), CertificateAlarm::Warning);
        assert_eq!(alarm_for(nb, na, 751, true), CertificateAlarm::Critical);
        assert_eq!(alarm_for(nb, na, 999, true), CertificateAlarm::Critical);
        assert_eq!(alarm_for(nb, na, 1000, true), CertificateAlarm::Expired);
        assert_eq!(alarm_for(nb, na, 1000, false), CertificateAlarm::Expired);
    }

    #[test]
    fn reverse_names_follow_rfc_8738() {
        assert_eq!(
            reverse_name("1.2.3.4".parse().unwrap()),
            "4.3.2.1.in-addr.arpa"
        );
        assert_eq!(
            reverse_name("2001:db8::1".parse().unwrap()),
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa"
        );
    }

    #[test]
    fn problem_types_map_to_categories() {
        let problem = |t: &str| {
            instant_acme::Error::Api(
                serde_json::from_value(serde_json::json!({
                    "type": format!("urn:ietf:params:acme:error:{t}"),
                    "detail": "x"
                }))
                .unwrap(),
            )
        };
        assert_eq!(
            acme_category(&problem("externalAccountRequired"), AcmeError::Authority),
            AcmeError::Eab
        );
        assert_eq!(
            acme_category(&problem("rateLimited"), AcmeError::Challenge),
            AcmeError::Authority
        );
        assert_eq!(
            acme_category(&problem("incorrectResponse"), AcmeError::Authority),
            AcmeError::Challenge
        );
        let http = instant_acme::Error::Other(Box::new(HttpFailure(AcmeError::Egress)));
        assert_eq!(
            acme_category(&http, AcmeError::Authority),
            AcmeError::Egress
        );
    }

    #[test]
    fn names_compare_case_and_order_insensitively() {
        assert_eq!(
            normalized(&["B.example".into(), "a.example".into(), "::0001".into()]),
            normalized(&["::1".into(), "b.example".into(), "a.example".into()])
        );
    }
}
