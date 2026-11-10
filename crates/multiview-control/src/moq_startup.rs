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
