//! The network front door wiring (ADR-0107 slice 1, issue #287).
//!
//! With `[network]` configured, one dual-stack UDP socket on
//! `[::]:network.https_port`, owned by `multiview-frontdoor` on its **own**
//! Tokio runtime, carries both MoQ (the vendored noq server, QUIC lane) and
//! WebRTC (the `UnifiedEndpoint`, WebRTC lane). Nothing it starts runs on the
//! `#[tokio::main]` runtime that drives the output clock.
//!
//! Without `[network]` nothing changes: WebRTC binds `webrtc.udp_port`, MoQ
//! binds `MULTIVIEW_MOQ_BIND`, both on the main runtime as before. Old configs
//! start unchanged.

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
    let max_rtc = usize::try_from(config.webrtc.max_sessions)
        .unwrap_or(usize::MAX)
        // Ingest publishers and whip_push sessions are admitted outside the
        // viewer pool; give them headroom in the established set.
        .saturating_add(64);
    Some(multiview_frontdoor::FrontDoorConfig {
        port: network.https_port,
        workers: usize::from(network.workers.max(1)),
        max_rtc_peers: max_rtc,
        ..multiview_frontdoor::FrontDoorConfig::default()
    })
}

/// Warn about the legacy keys `[network]` deprecates (one release, then
/// rejected; design note section 10). Never changes behaviour.
pub fn warn_deprecated(config: &MultiviewConfig) {
    let Some(network) = config.network.as_ref() else {
        if config.webrtc.udp_port != 8189 {
            tracing::info!(
                udp_port = config.webrtc.udp_port,
                "webrtc.udp_port is deprecated in favour of [network] https_port (ADR-0107); \
                 it keeps working without [network]"
            );
        }
        return;
    };
    if config.webrtc.udp_port != 8189 {
        tracing::warn!(
            udp_port = config.webrtc.udp_port,
            https_port = network.https_port,
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
            runtime: self.door.handle(),
            peers: Some(peers),
        })
    }

    /// The counters (telemetry).
    #[must_use]
    pub fn stats(&self) -> Arc<multiview_frontdoor::Stats> {
        self.door.stats()
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
        let defaults = load(&format!("{BASE}\n[network]\n"));
        let fd = config_from(&defaults).unwrap();
        assert_eq!((fd.port, fd.workers), (443, 2));
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
