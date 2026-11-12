//! Front-door ACME boot wiring (ADR-0107 section 5a, issue #291 slice 5).
//!
//! With `network.public_host` + `[network.tls]` configured, [`TlsBoot::start`]
//! runs at `multiview run` start, before the control plane binds and never on
//! the output-clock path:
//!
//! 1. a fresh bootstrap certificate (P-256, SAN = public host, 7 days); its key
//!    is persisted 0600 in `state_dir` and its SHA-256 fingerprint logged;
//! 2. the [`AcmeCertificateSource`] (state `setup`) and its
//!    [`ChallengeResolver`] (TLS-ALPN-01 aware);
//! 3. the ACME engine (Cloudflare DNS-01 provider when configured) on its own
//!    thread + runtime ([`AcmeService`]): a valid stored certificate is
//!    installed first without contacting the CA, otherwise issuance starts at
//!    once; the bootstrap certificate is rotated every 6 days while in setup;
//! 4. the alarm hook raises / clears the `tls-certificate` health warning
//!    (the warning store the UI already shows under `GET /api/v1/health`);
//! 5. a one-time setup code, printed on stderr only (the host's journal),
//!    never through `tracing`, so it never reaches the `GET /api/v1/logs` ring.
//!
//! A missing credential is not a startup error: the engine stays in `setup`
//! and `GET /api/v1/tls/status` names the missing variable.

use std::sync::Arc;
use std::time::Duration;

use multiview_config::{DnsProviderConfig, MultiviewConfig, NetworkTlsConfig};
use multiview_control::acme::cloudflare::{CloudflareCredential, CloudflareDns, SecretLookup};
use multiview_control::acme::{AcmeEngine, AcmeHandle, AcmeService, AlarmHook, DnsProvider};
use multiview_control::certificate_source::{
    bootstrap_certificate, AcmeCertificateSource, CertificateAlarm, CertificateSource,
    ChallengeResolver,
};
use multiview_control::tls_setup::{SetupAuth, TlsContext};
use multiview_engine::EnginePublisher;
use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};

/// Bootstrap rotation period while in setup (the certificate lives 7 days).
pub const BOOTSTRAP_ROTATION: Duration = Duration::from_secs(6 * 24 * 60 * 60);

/// The running ACME wiring for one `multiview run`.
pub struct TlsBoot {
    /// What the control plane and the front door share.
    pub context: Arc<TlsContext>,
    /// The ACME service; dropping it stops the thread (bounded join).
    handle: AcmeHandle,
}

impl std::fmt::Debug for TlsBoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsBoot").finish_non_exhaustive()
    }
}

/// The `[network]` ACME settings, when configured: `(public_host, tls)`.
#[must_use]
pub fn configured(config: &MultiviewConfig) -> Option<(&str, &NetworkTlsConfig)> {
    let network = config.network.as_ref()?;
    Some((network.public_host.as_deref()?, network.tls.as_ref()?))
}

/// The `tls-certificate` health warning for an alarm level (`None` clears).
#[must_use]
pub fn alarm_warning(alarm: CertificateAlarm, since_nanos: i64) -> HealthWarning {
    let (severity, message, active) = match alarm {
        CertificateAlarm::None => (
            WarningSeverity::Info,
            "The front-door TLS certificate renewed; no action needed.",
            false,
        ),
        CertificateAlarm::Warning => (
            WarningSeverity::Warning,
            "Front-door TLS certificate renewal is failing and less than half of its lifetime remains.",
            true,
        ),
        CertificateAlarm::Critical => (
            WarningSeverity::Critical,
            "Front-door TLS certificate renewal is failing and less than a quarter of its lifetime remains.",
            true,
        ),
        CertificateAlarm::Expired => (
            WarningSeverity::Critical,
            "The front-door TLS certificate has expired; browsers will refuse it until a renewal succeeds.",
            true,
        ),
    };
    HealthWarning {
        code: WarningCode::TlsCertificate,
        severity,
        subsystem: "tls".to_owned(),
        message: message.to_owned(),
        remediation:
            "Run `multiview tls status` on the host for the reason, fix the credential or \
                      CA reachability, then `multiview tls issue` (runbook: acme-certificates.md)."
                .to_owned(),
        since: since_nanos,
        active,
    }
}

fn now_nanos() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    )
    .unwrap_or(i64::MAX)
}

/// The alarm hook: publish raise/clear through the engine's drop-oldest event
/// lane (never blocks; inv #10). The warning ingest mirrors it into the store.
#[must_use]
pub fn alarm_hook(
    publisher: Arc<EnginePublisher<multiview_control::EngineStateSnapshot, Event>>,
) -> AlarmHook {
    Arc::new(move |alarm| {
        let warning = alarm_warning(alarm, now_nanos());
        let event = if warning.active {
            Event::HealthWarningRaised(warning)
        } else {
            Event::HealthWarningCleared(warning)
        };
        publisher.publish_event(event);
    })
}

/// The DNS-01 provider, when the challenge needs one.
fn dns_provider(tls: &NetworkTlsConfig) -> Option<Arc<dyn DnsProvider>> {
    let dns = tls.dns.clone()?;
    Some(Arc::new(CredentialReloading {
        config: dns,
        secrets: SecretLookup::new(&tls.state_dir),
        current: std::sync::Mutex::new(None),
    }))
}

/// Cloudflare with the credential (re)loaded at the start of every attempt
/// (`validate_credential` is the first call of each order). A credential
/// missing at boot keeps the engine in `setup` with `dns-credential-rejected`;
/// `multiview tls set-credential` + `multiview tls issue` then proceed without
/// a restart, and a rotated credential is picked up the same way.
struct CredentialReloading {
    config: DnsProviderConfig,
    secrets: SecretLookup,
    current: std::sync::Mutex<Option<Arc<CloudflareDns>>>,
}

impl CredentialReloading {
    fn reload(&self) -> Result<Arc<CloudflareDns>, multiview_control::acme::AcmeError> {
        let provider = Arc::new(
            CloudflareCredential::from_config(&self.config, &self.secrets)
                .and_then(CloudflareDns::new)?,
        );
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&provider));
        Ok(provider)
    }

    fn get(&self) -> Result<Arc<CloudflareDns>, multiview_control::acme::AcmeError> {
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match current {
            Some(provider) => Ok(provider),
            None => self.reload(),
        }
    }
}

impl DnsProvider for CredentialReloading {
    fn name(&self) -> &'static str {
        "cloudflare"
    }
    fn validate_credential<'a>(
        &'a self,
        names: &'a [String],
    ) -> multiview_control::acme::DnsFuture<'a, ()> {
        Box::pin(async move {
            let provider = self.reload()?;
            provider.validate_credential(names).await
        })
    }
    fn publish_txt<'a>(
        &'a self,
        fqdn: &'a str,
        value: &'a str,
    ) -> multiview_control::acme::DnsFuture<'a, multiview_control::acme::TxtHandle> {
        Box::pin(async move { self.get()?.publish_txt(fqdn, value).await })
    }
    fn confirm_at_authoritative<'a>(
        &'a self,
        fqdn: &'a str,
        value: &'a str,
        deadline: Duration,
    ) -> multiview_control::acme::DnsFuture<'a, ()> {
        Box::pin(async move {
            self.get()?
                .confirm_at_authoritative(fqdn, value, deadline)
                .await
        })
    }
    fn remove_txt<'a>(
        &'a self,
        handle: &'a multiview_control::acme::TxtHandle,
    ) -> multiview_control::acme::DnsFuture<'a, ()> {
        Box::pin(async move { self.get()?.remove_txt(handle).await })
    }
}

/// Knobs for tests (shorter deadlines); production uses [`Default`].
#[derive(Debug, Clone, Default)]
pub struct BootOptions {
    /// Override the CA connection deadlines.
    pub egress_timeouts: Option<multiview_control::acme::egress::EgressTimeouts>,
    /// Override the whole-attempt deadline.
    pub attempt_timeout: Option<Duration>,
}

impl TlsBoot {
    /// Start the ACME wiring, or `Ok(None)` when `[network.tls]` is absent.
    ///
    /// Blocking (key generation, one fsync): call before the output clock
    /// starts, or from `spawn_blocking`.
    ///
    /// # Errors
    /// The bootstrap certificate cannot be created or persisted, the engine
    /// configuration is invalid, or the service thread cannot be spawned.
    pub fn start(
        config: &MultiviewConfig,
        publisher: Option<Arc<EnginePublisher<multiview_control::EngineStateSnapshot, Event>>>,
        options: &BootOptions,
    ) -> anyhow::Result<Option<Self>> {
        let Some((public_host, tls)) = configured(config) else {
            return Ok(None);
        };
        let bootstrap = bootstrap_certificate(public_host)
            .map_err(|_| anyhow::anyhow!("tls: could not create the bootstrap certificate"))?;
        multiview_control::acme::store::private_dir(&tls.state_dir).map_err(|_| {
            anyhow::anyhow!(
                "tls: network.tls.state_dir {} must be an owner-private 0700 directory",
                tls.state_dir.display()
            )
        })?;
        bootstrap.persist(&tls.state_dir).map_err(|_| {
            anyhow::anyhow!("tls: could not persist the bootstrap key in network.tls.state_dir")
        })?;
        tracing::info!(
            fingerprint = %bootstrap.fingerprint,
            public_host,
            "tls: front door in setup with a self-generated bootstrap certificate; compare this \
             SHA-256 fingerprint with the one your browser shows"
        );
        let ca = match tls.ca {
            multiview_config::CertificateAuthority::Letsencrypt => "letsencrypt",
            multiview_config::CertificateAuthority::Google => "google",
            multiview_config::CertificateAuthority::Custom => "custom",
        };
        let source = Arc::new(AcmeCertificateSource::new(
            Arc::clone(&bootstrap.certificate),
            bootstrap.fingerprint.clone(),
            ca.to_owned(),
            tls.domains.clone(),
        ));
        let certificates: Arc<dyn CertificateSource> = source.clone();
        let resolver = Arc::new(ChallengeResolver::new(Arc::clone(&certificates)));
        let mut engine = AcmeEngine::new(
            tls.clone(),
            dns_provider(tls),
            Arc::clone(&resolver),
            Arc::clone(&source),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "tls: ACME engine configuration rejected ({})",
                e.as_reason()
            )
        })?
        .with_bootstrap_rotation(public_host.to_owned(), BOOTSTRAP_ROTATION);
        if let Some(publisher) = publisher {
            engine = engine.with_alarm_hook(alarm_hook(publisher));
        }
        if let Some(timeouts) = options.egress_timeouts {
            engine = engine.with_egress_timeouts(timeouts);
        }
        if let Some(timeout) = options.attempt_timeout {
            engine = engine.with_attempt_timeout(timeout);
        }
        let handle = AcmeService::spawn(engine)?;
        let setup = Arc::new(SetupAuth::new());
        let code = setup.mint();
        // The code goes to stderr (the host's journal) only, never through
        // `tracing`: the log ring is served by `GET /api/v1/logs` to any Read
        // principal, and the code must not leave the host by any route.
        print_setup_code(&code);
        tracing::info!(
            "tls: one-time setup code printed on stderr (valid 1 h, single use; \
             `multiview tls setup-code` mints a new one)"
        );
        // Section 5a: behind a reverse proxy TLS-ALPN-01 cannot reach the
        // engine (the proxy terminates or routes TLS), so status reports the
        // hint.
        let behind_proxy = config.network.as_ref().and_then(|n| n.mode)
            == Some(multiview_config::NetworkMode::BehindProxy);
        if config.network.as_ref().and_then(|n| n.mode).is_none() {
            tracing::warn!(
                "tls: [network.tls] is configured but network.mode is not set, so the front \
                 door has no TCP door: the setup page and TLS-ALPN-01 are unreachable from \
                 outside and the certificate is used only by QUIC; set network.mode \
                 (\"direct\" or \"behind_proxy\") with network.https_port"
            );
        }
        let context = Arc::new(TlsContext {
            source,
            certificates,
            resolver,
            issue: handle.trigger(),
            config: Arc::new(tls.clone()),
            setup,
            behind_proxy,
        });
        Ok(Some(Self { context, handle }))
    }

    /// The resolver slice 4's TCP 443 listener takes (TLS-ALPN-01 aware).
    #[must_use]
    pub fn resolver(&self) -> Arc<ChallengeResolver> {
        Arc::clone(&self.context.resolver)
    }

    /// Whether the ACME service thread has exited (it should not, until drop).
    #[must_use]
    pub fn service_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

fn installed() -> &'static std::sync::Mutex<Option<Arc<TlsContext>>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<Arc<TlsContext>>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install (or with `None` clear) the run's TLS context for the control plane
/// (mirrors `output_routing_control::install_output_routing`): `bind_and_serve`
/// puts it into the `AppState` via `with_tls`.
pub fn install_tls_context(context: Option<Arc<TlsContext>>) {
    *installed()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = context;
}

/// The installed TLS context, if any.
#[must_use]
pub fn installed_tls_context() -> Option<Arc<TlsContext>> {
    installed()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Wait (bounded by `shutdown`) until `source` is serving. Returns `false`
/// if `shutdown` fired first. Used to hold the front-door QUIC lane until the
/// ACME certificate is installed (section 5a: QUIC is not started in setup).
pub async fn wait_serving(
    source: Arc<dyn CertificateSource>,
    shutdown: impl std::future::Future<Output = ()>,
) -> bool {
    use multiview_control::certificate_source::TlsState;
    let mut changes = source.subscribe();
    tokio::pin!(shutdown);
    loop {
        if source.status().state == TlsState::Serving {
            return true;
        }
        tokio::select! {
            () = &mut shutdown => return false,
            result = changes.changed() => {
                if result.is_err() {
                    // The source is gone: nothing will ever serve.
                    return false;
                }
            }
        }
    }
}

/// Print the one-time setup code on stderr, bypassing `tracing` (and so the
/// log ring that `GET /api/v1/logs` serves).
#[allow(clippy::print_stderr)]
fn print_setup_code(code: &str) {
    eprintln!("multiview tls: one-time setup code (valid 1 h, single use): {code}");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn alarm_warnings_map_levels_and_clear() {
        assert!(!alarm_warning(CertificateAlarm::None, 1).active);
        let w = alarm_warning(CertificateAlarm::Warning, 1);
        assert!(w.active);
        assert_eq!(w.severity, WarningSeverity::Warning);
        assert_eq!(w.code.as_str(), "tls-certificate");
        assert_eq!(
            alarm_warning(CertificateAlarm::Critical, 1).severity,
            WarningSeverity::Critical
        );
        assert_eq!(
            alarm_warning(CertificateAlarm::Expired, 1).severity,
            WarningSeverity::Critical
        );
    }

    #[tokio::test]
    async fn wait_serving_returns_on_install_or_shutdown() {
        let boot = bootstrap_certificate("a.example.org").unwrap();
        let source = Arc::new(AcmeCertificateSource::new(
            boot.certificate.clone(),
            boot.fingerprint,
            "custom".into(),
            vec!["a.example.org".into()],
        ));
        // Shutdown first: false.
        assert!(!wait_serving(source.clone(), async {}).await);
        // Install while waiting: true.
        let dir = tempfile::tempdir().unwrap();
        let waiter = tokio::spawn(wait_serving(source.clone(), std::future::pending()));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        let next = bootstrap_certificate("a.example.org").unwrap();
        source
            .install(next.certificate, i64::MAX, dir.path())
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap());
    }
}
