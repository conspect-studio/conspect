//! The network front door wiring (ADR-0107 slice 1, issue #287).
//!
//! With `network.https_port` configured, one dual-stack UDP socket on
//! `[::]:network.https_port`, owned by `multiview-frontdoor` on its **own**
//! Tokio runtime, carries both MoQ (the vendored noq server, QUIC lane) and
//! WebRTC (the `UnifiedEndpoint`, WebRTC lane). The socket, the dispatcher,
//! the noq endpoint driver and every QUIC connection task, the MoQ accept loop
//! and its sessions, and the WebRTC driver run on the front-door runtime.
//!
//! Not moved in slice 1 (pre-existing, on the `#[tokio::main]` runtime): the MoQ
//! per-admission publication pump and its `moq-net` origin driver, spawned by
//! `Runtime::admit*` when the control plane admits a viewer, and the registry's
//! lease-expiry task. They are fed by bounded drop-oldest windows and never
//! touch the socket; moving them is follow-up work.
//!
//! Without it nothing changes: WebRTC binds `webrtc.udp_port`, MoQ
//! binds `MULTIVIEW_MOQ_BIND`, both on the main runtime as before. Old configs
//! start unchanged.
//!
//! **Certificates (slice 5, ADR-0107 section 5a).** With `[network.tls]` the
//! run starts the ACME wiring ([`crate::tls_boot`]) before the control plane
//! binds, and:
//!
//! * the QUIC lane is held: MoQ starts on it only once the certificate source
//!   is `serving` ([`start_moq_when_serving`]), and then serves the ACME
//!   certificate through `SourceResolver` (renewals reach new handshakes with
//!   no restart), never the `MULTIVIEW_MOQ_CERTIFICATE` files;
//! * slice 4's TCP 443 listener (not built yet) takes the TLS-ALPN-01-aware
//!   resolver from [`front_door_tls`] and mounts
//!   `multiview_control::tls_setup::front_door_router(state)` (the control
//!   router behind the setup gate). The `[control] listen` listener stays the
//!   loopback admin listener and is not gated.

use std::sync::Arc;

use multiview_config::MultiviewConfig;

/// The running front door and the lanes its consumers have not taken yet.
pub struct FrontDoorRun {
    door: multiview_frontdoor::FrontDoor,
}

impl std::fmt::Debug for FrontDoorRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontDoorRun")
            .field("local_addr", &self.door.local_addr())
            .finish()
    }
}

/// The settings derived from `[network]` (and the legacy keys it replaces).
#[must_use]
pub fn config_from(config: &MultiviewConfig) -> Option<multiview_frontdoor::FrontDoorConfig> {
    let network = config.network.as_ref()?;
    // `[network]` with only ACME settings (slice 5) does not start the socket.
    let port = network.https_port?;
    let max_rtc = usize::try_from(config.webrtc.max_sessions)
        .unwrap_or(usize::MAX)
        // Ingest publishers and whip_push sessions are admitted outside the
        // viewer pool; give them headroom in the established set.
        .saturating_add(64);
    // TURN servers the WebRTC endpoint allocates on: only their datagrams may be
    // ChannelData (64-79), and they are trusted by configuration (relayed
    // media from them is on the established path).
    #[cfg(feature = "webrtc-native")]
    let turn_servers = crate::webrtc_endpoint::endpoint_config_from(config)
        .turn_servers()
        .map(|s| s.addr)
        .collect();
    #[cfg(not(feature = "webrtc-native"))]
    let turn_servers = Vec::new();
    Some(multiview_frontdoor::FrontDoorConfig {
        port,
        workers: usize::from(network.workers.max(1)),
        max_rtc_peers: max_rtc,
        turn_servers,
        ..multiview_frontdoor::FrontDoorConfig::default()
    })
}

/// Warn about the legacy keys `[network]` deprecates (one release, then
/// rejected; design note section 10). Never changes behaviour.
pub fn warn_deprecated(config: &MultiviewConfig) {
    let Some(network) = config.network.as_ref().filter(|n| n.https_port.is_some()) else {
        if config.webrtc.udp_port != 8189 {
            tracing::info!(
                udp_port = config.webrtc.udp_port,
                "webrtc.udp_port is deprecated in favour of [network] https_port (ADR-0107); \
                 it keeps working while network.https_port is unset"
            );
        }
        return;
    };
    if config.webrtc.udp_port != 8189 {
        tracing::warn!(
            udp_port = config.webrtc.udp_port,
            https_port = ?network.https_port,
            "webrtc.udp_port is deprecated and ignored while [network] is configured: \
             WebRTC rides the front-door socket (ADR-0107)"
        );
    }
    if !config.webrtc.advertised_addresses.is_empty() && !network.advertise.is_empty() {
        tracing::warn!(
            "webrtc.advertised_addresses is deprecated; network.advertise wins (ADR-0107)"
        );
    } else if !config.webrtc.advertised_addresses.is_empty() {
        tracing::warn!(
            "webrtc.advertised_addresses is deprecated; move it to network.advertise (ADR-0107)"
        );
    }
}

impl FrontDoorRun {
    /// Start the front door if `[network]` is configured. A bind failure is
    /// logged and returns `None`: the run continues with WebRTC and `MoQ`
    /// unavailable rather than silently falling back to the legacy ports.
    #[must_use]
    pub fn start(config: &MultiviewConfig) -> Option<Self> {
        warn_deprecated(config);
        let fd_config = config_from(config)?;
        match multiview_frontdoor::FrontDoor::start(&fd_config) {
            Ok(door) => Some(Self { door }),
            Err(error) => {
                tracing::error!(%error, port = fd_config.port, "the front door failed to start; WebRTC and MoQ unavailable this run");
                None
            }
        }
    }

    /// The bound address.
    #[must_use]
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.door.local_addr()
    }

    /// The front door's runtime handle.
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.door.handle()
    }

    /// Take the WebRTC lane.
    #[cfg(feature = "webrtc-native")]
    pub fn take_rtc_lane(&mut self) -> Option<multiview_frontdoor::RtcLane> {
        self.door.take_rtc_lane().ok()
    }

    /// Take the QUIC lane, wrapped for the `MoQ` listener.
    #[cfg(feature = "moq-output")]
    pub fn take_moq_socket(&mut self) -> Option<multiview_moq::provider::SharedSocket> {
        let lane = self.door.take_quic_lane().ok()?;
        let established = self.door.quic_established();
        let peers: multiview_moq::provider::PeerHook = Arc::new(move |peer, up| {
            if up {
                established.insert(peer);
            } else {
                established.remove(peer);
            }
        });
        Some(multiview_moq::provider::SharedSocket {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
            incoming: Some(multiview_frontdoor::quic::incoming_caps()),
            runtime: self.door.handle(),
            peers: Some(peers),
        })
    }

    /// Shut the front door down: its runtime (dispatcher, QUIC and WebRTC
    /// tasks) stops and the socket is closed, waiting at most
    /// [`multiview_frontdoor::SHUTDOWN_TIMEOUT`] on a blocking-pool thread.
    pub async fn shutdown(self) {
        let door = self.door;
        if tokio::task::spawn_blocking(move || door.shutdown_blocking())
            .await
            .is_err()
        {
            tracing::warn!("front-door shutdown task failed");
        }
    }

    /// The counters (telemetry).
    #[must_use]
    pub fn stats(&self) -> Arc<multiview_frontdoor::Stats> {
        self.door.stats()
    }
}

/// The resolver slice 4's TCP 443 listener installs (ACME certificate, with
/// TLS-ALPN-01 challenges answered for ALPN `acme-tls/1`), when the run has
/// `[network.tls]`. Mount it with `multiview_control::tls_setup::front_door_router`.
#[cfg(all(feature = "acme", unix))]
#[must_use]
pub fn front_door_tls() -> Option<Arc<multiview_control::certificate_source::ChallengeResolver>> {
    crate::tls_boot::installed_tls_context().map(|tls| Arc::clone(&tls.resolver))
}

/// Hold the front-door QUIC lane until the ACME source is serving, then start
/// MoQ on it with the ACME certificate and install the real provider into
/// `deferred`. Bounded by `shutdown`; a control-plane task, never on the
/// output clock (it only awaits a watch channel).
#[cfg(all(feature = "acme", unix, feature = "moq-output"))]
pub async fn start_moq_when_serving(
    tls: Arc<multiview_control::tls_setup::TlsContext>,
    shutdown: impl std::future::Future<Output = ()>,
    endpoint: String,
    protocol: String,
    factory: Option<Arc<dyn multiview_control::moq_provider::AuthorizedFeedFactory>>,
    socket: multiview_moq::provider::SharedSocket,
    deferred: Arc<multiview_control::moq_startup::DeferredProvider>,
) {
    tracing::info!("front door: QUIC/MoQ held until the ACME certificate is serving");
    if !crate::tls_boot::wait_serving(Arc::clone(&tls.certificates), shutdown).await {
        return;
    }
    let resolver: Arc<dyn multiview_moq::moq_tokio_rustls::server::ResolvesServerCert> = Arc::new(
        multiview_control::certificate_source::SourceResolver(Arc::clone(&tls.certificates)),
    );
    match multiview_control::moq_startup::start_shared_resolver(
        endpoint, protocol, factory, socket, resolver,
    )
    .await
    {
        Ok(provider) => {
            deferred.set(provider);
            tracing::info!("front door: certificate serving; QUIC/MoQ started");
        }
        Err(_) => tracing::error!("front door: MoQ listener failed to start on the QUIC lane"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BASE: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "25/1"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
"##;

    fn load(text: &str) -> MultiviewConfig {
        MultiviewConfig::load_from_toml(text).expect("config parses")
    }

    #[test]
    fn absent_network_keeps_the_legacy_listeners() {
        // An old config (custom webrtc.udp_port, no [network]) starts unchanged.
        let config = load(&format!("{BASE}\n[webrtc]\nudp_port = 9189\n"));
        assert!(config_from(&config).is_none());
        assert_eq!(config.webrtc.udp_port, 9189);
    }

    #[test]
    fn network_section_selects_the_front_door() {
        let config = load(&format!(
            "{BASE}\n[network]\nhttps_port = 8443\nworkers = 3\n"
        ));
        let fd = config_from(&config).expect("front door configured");
        assert_eq!(fd.port, 8443);
        assert_eq!(fd.workers, 3);
        let defaults = load(&format!("{BASE}\n[network]\nhttps_port = 443\n"));
        let fd = config_from(&defaults).unwrap();
        assert_eq!((fd.port, fd.workers), (443, 2));
        // A [network] section without https_port (ACME settings only) keeps
        // the legacy listeners.
        let acme_only = load(&format!("{BASE}\n[network]\nworkers = 2\n"));
        assert!(config_from(&acme_only).is_none());
    }

    #[test]
    fn invalid_network_section_is_rejected_by_validation() {
        for bad in ["https_port = 0", "workers = 0"] {
            let config = load(&format!("{BASE}\n[network]\n{bad}\n"));
            let network = config.network.as_ref().expect("[network] parsed");
            let err = network.validate().expect_err(bad).to_string();
            assert!(err.contains("network."), "{bad}: {err}");
        }
    }
}
