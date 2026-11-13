//! Explicit optional listener configuration; no synthetic production feed.
use crate::{
    moq::{MoqRegistry, MoqReject},
    moq_provider::{AuthorizedFeedFactory, WebTransportProvider},
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

/// Trusted listener inputs, supplied by the service configuration or secret-file references.
pub struct MoqListenerConfig {
    /// IPv6-first QUIC bind address.
    pub bind: SocketAddr,
    /// Public HTTPS endpoint, without credentials, query, or fragment.
    pub endpoint: String,
    /// Explicit `moq-transport-16` or `moq-transport-18`.
    pub protocol: String,
    /// PEM certificate-chain file.
    pub certificate: PathBuf,
    /// Private PEM key file; never put the key in arguments or diagnostics.
    pub private_key: PathBuf,
}

impl MoqListenerConfig {
    /// Read explicit opt-in listener settings. Absent endpoint disables `MoQ`;
    /// partial or unsafe settings fail instead of enabling an insecure listener.
    /// # Errors
    /// Returns unavailable for incomplete or unsafe explicit listener settings.
    pub fn from_env() -> Result<Option<Self>, MoqReject> {
        Self::from_values(|name| std::env::var(name).ok())
    }
    /// [`from_env`](Self::from_env) for the shared front door (ADR-0107 slice
    /// 1): `MULTIVIEW_MOQ_BIND` is not needed (`MoQ` rides the front-door socket);
    /// when present it is ignored with a deprecation warning. `bind` is set to
    /// the front door's address.
    ///
    /// # Errors
    ///
    /// As for [`from_env`](Self::from_env).
    pub fn from_env_shared(front_door: SocketAddr) -> Result<Option<Self>, MoqReject> {
        let get = |name: &str| std::env::var(name).ok();
        if get("MULTIVIEW_MOQ_BIND").is_some() {
            tracing::warn!(
                front_door = %front_door,
                "MULTIVIEW_MOQ_BIND is deprecated and ignored while [network] is configured:                  MoQ rides the front-door socket (ADR-0107); remove it"
            );
        }
        let bind = front_door.to_string();
        Self::from_values(|name| {
            if name == "MULTIVIEW_MOQ_BIND" {
                Some(bind.clone())
            } else {
                get(name)
            }
        })
    }

    /// `(endpoint, protocol)` for a listener on the front door whose
    /// certificate comes from the ACME source (ADR-0107 section 5a):
    /// `MULTIVIEW_MOQ_ENDPOINT` and `MULTIVIEW_MOQ_PROTOCOL` with the same
    /// checks as [`from_env`](Self::from_env); the certificate files are not
    /// read (and are ignored with a warning when set).
    ///
    /// # Errors
    /// Returns unavailable for incomplete or unsafe explicit settings.
    pub fn endpoint_from_env() -> Result<Option<(String, String)>, MoqReject> {
        Self::endpoint_from_values(|name| std::env::var(name).ok())
    }

    fn endpoint_from_values(
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<(String, String)>, MoqReject> {
        let Some(endpoint) = get("MULTIVIEW_MOQ_ENDPOINT") else {
            return Ok(None);
        };
        let url = url::Url::parse(&endpoint).map_err(|_| MoqReject::Unavailable)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(MoqReject::Unavailable);
        }
        let protocol = get("MULTIVIEW_MOQ_PROTOCOL").ok_or(MoqReject::Unavailable)?;
        if !matches!(protocol.as_str(), "moq-transport-16" | "moq-transport-18") {
            return Err(MoqReject::Unavailable);
        }
        if get("MULTIVIEW_MOQ_CERTIFICATE").is_some() || get("MULTIVIEW_MOQ_PRIVATE_KEY").is_some()
        {
            tracing::warn!(
                "MULTIVIEW_MOQ_CERTIFICATE/_PRIVATE_KEY are ignored while [network.tls] supplies \
                 the front-door certificate (ADR-0107)"
            );
        }
        Ok(Some((endpoint, protocol)))
    }

    fn from_values(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, MoqReject> {
        let Some(endpoint) = get("MULTIVIEW_MOQ_ENDPOINT") else {
            return Ok(None);
        };
        let url = url::Url::parse(&endpoint).map_err(|_| MoqReject::Unavailable)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(MoqReject::Unavailable);
        }
        let bind = get("MULTIVIEW_MOQ_BIND")
            .ok_or(MoqReject::Unavailable)?
            .parse::<SocketAddr>()
            .map_err(|_| MoqReject::Unavailable)?;
        if !bind.is_ipv6() {
            return Err(MoqReject::Unavailable);
        }
        let protocol = get("MULTIVIEW_MOQ_PROTOCOL").ok_or(MoqReject::Unavailable)?;
        if !matches!(protocol.as_str(), "moq-transport-16" | "moq-transport-18") {
            return Err(MoqReject::Unavailable);
        }
        let certificate =
            PathBuf::from(get("MULTIVIEW_MOQ_CERTIFICATE").ok_or(MoqReject::Unavailable)?);
        let private_key =
            PathBuf::from(get("MULTIVIEW_MOQ_PRIVATE_KEY").ok_or(MoqReject::Unavailable)?);
        if !certificate.is_file() || !private_key.is_file() {
            return Err(MoqReject::Unavailable);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata =
                std::fs::symlink_metadata(&private_key).map_err(|_| MoqReject::Unavailable)?;
            if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(MoqReject::Unavailable);
            }
        }
        Ok(Some(Self {
            bind,
            endpoint,
            protocol,
            certificate,
            private_key,
        }))
    }
}

/// [`start`] on the shared front-door socket (ADR-0107 slice 1): the
/// listener binds nothing; it serves `MoQ` from the front door's QUIC lane, on the
/// front door's own runtime.
///
/// # Errors
///
/// As for [`start`].
pub async fn start_shared(
    config: MoqListenerConfig,
    factory: Option<Arc<dyn AuthorizedFeedFactory>>,
    on: multiview_moq::provider::SharedSocket,
) -> Result<Arc<MoqRegistry>, MoqReject> {
    let factory = factory.ok_or(MoqReject::Unavailable)?;
    let runtime = multiview_moq::provider::Runtime::listen_files_on(
        on,
        config.endpoint,
        &config.protocol,
        config.certificate,
        config.private_key,
    )
    .await
    .map_err(|_| MoqReject::Unavailable)?;
    Ok(Arc::new(MoqRegistry::new(
        Arc::new(WebTransportProvider::new(runtime, factory)),
        std::time::Duration::from_secs(30),
        16,
    )))
}

/// A provider whose listener starts later (ADR-0107 section 5a: the front-door
/// QUIC lane is not started while the certificate is in `setup`). Until
/// [`DeferredProvider::set`] runs it is unavailable, so every `MoQ` lease answers
/// the existing honest `503`; afterwards it forwards to the real provider.
/// One lock-free read per admission check.
#[derive(Default)]
pub struct DeferredProvider {
    inner: std::sync::OnceLock<Arc<dyn crate::moq::MoqProvider>>,
}

impl DeferredProvider {
    /// Install the real provider (first call wins).
    pub fn set(&self, provider: Arc<dyn crate::moq::MoqProvider>) {
        let _ = self.inner.set(provider);
    }

    /// Whether the real provider is installed.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.inner.get().is_some()
    }
}

impl crate::moq::MoqProvider for DeferredProvider {
    fn available(&self) -> bool {
        self.inner.get().is_some_and(|p| p.available())
    }
    fn admit(
        &self,
        principal: &crate::auth::Principal,
        scope: &crate::preview::WhepScope,
        subscription_id: &str,
        ttl: std::time::Duration,
    ) -> Result<(crate::moq::MoqLease, Arc<dyn crate::moq::MoqFence>), MoqReject> {
        self.inner.get().ok_or(MoqReject::Unavailable)?.admit(
            principal,
            scope,
            subscription_id,
            ttl,
        )
    }
}

/// [`start_shared`] serving the certificate from `resolver` (the front door's
/// ACME source) instead of `MULTIVIEW_MOQ_CERTIFICATE`/`_PRIVATE_KEY` files.
///
/// # Errors
/// As for [`start_shared`].
pub async fn start_shared_resolver(
    endpoint: String,
    protocol: String,
    factory: Option<Arc<dyn AuthorizedFeedFactory>>,
    on: multiview_moq::provider::SharedSocket,
    resolver: Arc<dyn multiview_moq::moq_tokio_rustls::server::ResolvesServerCert>,
) -> Result<Arc<dyn crate::moq::MoqProvider>, MoqReject> {
    let factory = factory.ok_or(MoqReject::Unavailable)?;
    let runtime =
        multiview_moq::provider::Runtime::listen_resolver_on(on, endpoint, &protocol, resolver)
            .await
            .map_err(|_| MoqReject::Unavailable)?;
    Ok(Arc::new(WebTransportProvider::new(runtime, factory)))
}

/// `MULTIVIEW_MOQ_ENDPOINT` for the front door's HTTP/3 endpoint (slice 2):
/// WebTransport sessions are accepted only under `/moq/`, so the lease URL
/// must point there. A bare origin (`https://host` or `https://host/`) is
/// normalised to `https://host/moq` with a warning; any other path must be
/// `/moq` or under it.
///
/// # Errors
/// [`MoqReject::Unavailable`] for an unparsable endpoint or a path outside
/// `/moq`.
pub fn h3_endpoint(endpoint: &str) -> Result<String, MoqReject> {
    let mut url = url::Url::parse(endpoint).map_err(|_| MoqReject::Unavailable)?;
    let path = url.path().trim_end_matches('/').to_owned();
    if path.is_empty() {
        url.set_path("/moq");
        tracing::warn!(
            endpoint = %url,
            "MULTIVIEW_MOQ_ENDPOINT has no path; the front door serves MoQ over WebTransport \
             under /moq, so leases use this endpoint (set it explicitly to silence this)"
        );
        return Ok(url.to_string());
    }
    if path == "/moq" || path.starts_with("/moq/") {
        return Ok(endpoint.to_owned());
    }
    tracing::error!(
        "MULTIVIEW_MOQ_ENDPOINT must be https://<host>/moq (or under /moq) for the front door's \
         HTTP/3 endpoint"
    );
    Err(MoqReject::Unavailable)
}

/// The `MoQ` handler of the front door's HTTP/3 endpoint (slice 2): every
/// extended CONNECT under `/moq/` is admitted by its single-use `?ticket=`
/// BEFORE the `200` is sent ([`Runtime::claim_ticket`]: unknown, reused or
/// revoked -> `401`, over the session cap -> `429`, shutting down -> `503`),
/// then served as a `MoQ` session of exactly the configured version.
///
/// [`Runtime::claim_ticket`]: multiview_moq::provider::Runtime::claim_ticket
pub struct MoqWt {
    runtime: Arc<multiview_moq::provider::Runtime>,
    protocols: [&'static str; 1],
    peers: Option<multiview_moq::provider::PeerHook>,
}

impl std::fmt::Debug for MoqWt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoqWt")
            .field("protocols", &self.protocols)
            .finish_non_exhaustive()
    }
}

impl MoqWt {
    /// The handler over an external runtime ([`Runtime::external`]). `peers`
    /// is told `(peer, true/false)` while a session runs (the endpoint's own
    /// hook already covers the connection; pass `None` then).
    ///
    /// # Errors
    /// The runtime's protocol is not a `MoQ` version.
    ///
    /// [`Runtime::external`]: multiview_moq::provider::Runtime::external
    pub fn new(
        runtime: Arc<multiview_moq::provider::Runtime>,
        peers: Option<multiview_moq::provider::PeerHook>,
    ) -> Result<Self, MoqReject> {
        let protocol = runtime.wt_protocol().map_err(|_| MoqReject::Unavailable)?;
        Ok(Self {
            runtime,
            protocols: [protocol],
            peers,
        })
    }
}

impl multiview_h3::WtHandler for MoqWt {
    fn admit(
        &self,
        req: &axum::http::request::Parts,
        _peer: SocketAddr,
    ) -> Result<multiview_h3::AdmitToken, axum::http::StatusCode> {
        use multiview_moq::provider::ClaimReject;
        match self.runtime.claim_ticket(req.uri.query()) {
            Ok(claim) => Ok(Box::new(claim)),
            Err(ClaimReject::Unauthorized) => Err(axum::http::StatusCode::UNAUTHORIZED),
            Err(ClaimReject::Capacity) => Err(axum::http::StatusCode::TOO_MANY_REQUESTS),
            Err(ClaimReject::Unavailable) => Err(axum::http::StatusCode::SERVICE_UNAVAILABLE),
        }
    }

    fn accept(
        &self,
        session: multiview_h3::WtSession,
        token: multiview_h3::AdmitToken,
        _req: axum::http::request::Parts,
        peer: SocketAddr,
    ) -> multiview_h3::BoxFuture<'static, ()> {
        let runtime = Arc::clone(&self.runtime);
        let peers = self.peers.clone();
        Box::pin(async move {
            let Ok(claim) = token.downcast::<multiview_moq::provider::Claim>() else {
                return;
            };
            runtime
                .serve_webtransport(*claim, session, peer, peers)
                .await;
        })
    }

    fn protocols(&self) -> &[&'static str] {
        &self.protocols
    }

    /// The external runtime speaks exactly one `moqt-*` version, which is
    /// negotiated only through `wt-protocol`: a session without it would be
    /// closed at once, so it is refused before the ticket is claimed.
    fn requires_protocol(&self) -> bool {
        true
    }
}

/// The `MoQ` registry over an external runtime for the front door's HTTP/3
/// endpoint, and the [`MoqWt`] handler the endpoint runs sessions through.
/// `endpoint` is checked/normalised by [`h3_endpoint`].
///
/// # Errors
/// [`MoqReject::Unavailable`] for a missing factory, a bad endpoint or
/// protocol.
pub fn start_h3(
    endpoint: &str,
    protocol: &str,
    factory: Option<Arc<dyn AuthorizedFeedFactory>>,
) -> Result<(Arc<dyn crate::moq::MoqProvider>, Arc<MoqWt>), MoqReject> {
    let factory = factory.ok_or(MoqReject::Unavailable)?;
    let endpoint = h3_endpoint(endpoint)?;
    let runtime = multiview_moq::provider::Runtime::external(endpoint, protocol)
        .map_err(|_| MoqReject::Unavailable)?;
    let wt = Arc::new(MoqWt::new(Arc::clone(&runtime), None)?);
    Ok((Arc::new(WebTransportProvider::new(runtime, factory)), wt))
}

/// Start only with an actual authorized shared-rendition factory.
///
/// # Errors
/// Returns unavailable when the factory is absent/unready, configuration is invalid,
/// or the trusted listener cannot bind. It never enables a fixture implicitly.
pub async fn start(
    config: MoqListenerConfig,
    factory: Option<Arc<dyn AuthorizedFeedFactory>>,
) -> Result<Arc<MoqRegistry>, MoqReject> {
    // A run-scoped dynamic factory can be installed before the pipeline has
    // published its first effective snapshot. Provider availability stays false.
    let factory = factory.ok_or(MoqReject::Unavailable)?;
    if !config.bind.is_ipv6() {
        return Err(MoqReject::Unavailable);
    }
    let runtime = multiview_moq::provider::Runtime::listen_files(
        config.bind,
        config.endpoint,
        &config.protocol,
        config.certificate,
        config.private_key,
    )
    .await
    .map_err(|_| MoqReject::Unavailable)?;
    Ok(Arc::new(MoqRegistry::new(
        Arc::new(WebTransportProvider::new(runtime, factory)),
        std::time::Duration::from_secs(30),
        16,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_environment_is_disabled_or_rejected_without_implicit_fallback() {
        assert!(MoqListenerConfig::from_values(|_| None).unwrap().is_none());
        for endpoint in [
            "http://localhost",
            "https://user:secret@localhost",
            "https://localhost/?ticket=secret",
            "https://localhost/#fragment",
        ] {
            assert!(MoqListenerConfig::from_values(
                |name| (name == "MULTIVIEW_MOQ_ENDPOINT").then(|| endpoint.into())
            )
            .is_err());
        }
        assert!(MoqListenerConfig::from_values(|name| match name {
            "MULTIVIEW_MOQ_ENDPOINT" => Some("https://localhost".into()),
            "MULTIVIEW_MOQ_BIND" => Some("127.0.0.1:4443".into()),
            _ => None,
        })
        .is_err());
    }

    #[test]
    fn h3_endpoint_lives_under_moq() {
        assert_eq!(
            h3_endpoint("https://example.org").unwrap(),
            "https://example.org/moq"
        );
        assert_eq!(
            h3_endpoint("https://example.org/").unwrap(),
            "https://example.org/moq"
        );
        assert_eq!(
            h3_endpoint("https://example.org/moq").unwrap(),
            "https://example.org/moq"
        );
        assert_eq!(
            h3_endpoint("https://[::1]:8443/moq/a").unwrap(),
            "https://[::1]:8443/moq/a"
        );
        assert!(h3_endpoint("https://example.org/other").is_err());
        assert!(h3_endpoint("https://example.org/moqx").is_err());
        assert!(h3_endpoint("not a url").is_err());
    }

    #[tokio::test]
    async fn missing_factory_never_binds_or_loads_keys() {
        let config = MoqListenerConfig {
            bind: std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 0)),
            endpoint: "https://example.invalid".into(),
            protocol: "moq-transport-16".into(),
            certificate: "/does-not-exist/cert.pem".into(),
            private_key: "/does-not-exist/key.pem".into(),
        };
        assert!(matches!(
            start(config, None).await,
            Err(MoqReject::Unavailable)
        ));
    }
}
