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

/// Start only with an actual authorized shared-rendition factory.
///
/// # Errors
/// Returns unavailable when the factory is absent/unready, configuration is invalid,
/// or the trusted listener cannot bind. It never enables a fixture implicitly.
pub async fn start(
    config: MoqListenerConfig,
    factory: Option<Arc<dyn AuthorizedFeedFactory>>,
) -> Result<Arc<MoqRegistry>, MoqReject> {
    let factory = factory
        .filter(|factory| factory.available())
        .ok_or(MoqReject::Unavailable)?;
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
