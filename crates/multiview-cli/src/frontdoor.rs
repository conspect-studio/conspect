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
//! Slice 4 (issue #290): with `network.mode` set as well, the TCP door starts on
//! the same front-door runtime (`direct`: `[bind]:tcp_port` plus the port-80
//! redirect; `behind_proxy`: `proxy_listen`, PROXY v2 required). `mode` absent
//! means no TCP listener at all. The site on the door is the SAME control-plane
//! router and serve options as the plain `[control]` listener, over the
//! `[control.tls]` static certificate ([`FrontDoorRun::install_site`]).

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use multiview_config::{MultiviewConfig, NetworkMode};
use multiview_frontdoor::ListenSockets;

/// The sockets systemd passed (`LISTEN_FDS`), adopted by `main` before the
/// runtime starts and claimed by [`FrontDoorRun::start`] (slice 6, #292).
static INHERITED: Mutex<Option<ListenSockets>> = Mutex::new(None);

/// Record the sockets the service manager passed. `main` calls this once with
/// [`ListenSockets::from_env`], before any other thread exists.
pub fn set_inherited_sockets(sockets: ListenSockets) {
    *INHERITED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sockets);
}

fn take_inherited_sockets() -> ListenSockets {
    INHERITED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_default()
}

/// The running front door and the lanes its consumers have not taken yet.
pub struct FrontDoorRun {
    door: multiview_frontdoor::FrontDoor,
    /// The TCP door runs with ICE-TCP on (direct mode, `network.ice_tcp`).
    ice_tcp: bool,
}

/// The TCP door settings derived from `[network]` (slice 4): `None` unless
/// `network.mode` is set.
#[derive(Debug, Clone)]
pub struct TcpPlan {
    /// The door.
    pub door: multiview_frontdoor::TcpDoorConfig,
    /// The port-80 redirect (direct mode, `http_redirect_port != 0`).
    pub redirect: Option<multiview_frontdoor::RedirectConfig>,
    /// The mode, for the startup log.
    pub mode: NetworkMode,
}

/// The TCP door plan for a validated config.
#[must_use]
pub fn tcp_config_from(config: &MultiviewConfig) -> Option<TcpPlan> {
    let network = config.network.as_ref()?;
    let mode = network.mode?;
    network.https_port?;
    let turn_sni = network
        .public_host
        .as_deref()
        .filter(|h| h.parse::<IpAddr>().is_err())
        .map(|h| format!("turn.{h}"));
    let base = multiview_frontdoor::TcpDoorConfig {
        ice_tcp: network.ice_tcp_enabled(),
        turn_sni,
        ..multiview_frontdoor::TcpDoorConfig::default()
    };
    match mode {
        NetworkMode::Direct => {
            let bind = IpAddr::V6(network.bind_ip().ok()?);
            let port = network.effective_tcp_port()?;
            let redirect = network.effective_redirect_port().and_then(|redirect| {
                let mut cfg = multiview_frontdoor::RedirectConfig::new(
                    SocketAddr::new(bind, redirect),
                    network.public_host.clone()?,
                    port,
                );
                cfg.acme_webroot = network.acme_webroot.as_ref().map(std::path::PathBuf::from);
                Some(cfg)
            });
            Some(TcpPlan {
                door: multiview_frontdoor::TcpDoorConfig {
                    listen: SocketAddr::new(bind, port),
                    ..base
                },
                redirect,
                mode,
            })
        }
        NetworkMode::BehindProxy => {
            let trusted = network.trusted_proxy_list().ok()?;
            Some(TcpPlan {
                door: multiview_frontdoor::TcpDoorConfig {
                    listen: network.proxy_listen_addr().ok()?,
                    // Design note section 8: no ICE-TCP behind a proxy (the
                    // proxy's port is not ours to advertise as a candidate).
                    ice_tcp: false,
                    proxy: Some(multiview_frontdoor::TrustedProxies::new(
                        trusted.iter().map(|t| (t.addr, t.prefix)),
                    )),
                    ..base
                },
                redirect: None,
                mode,
            })
        }
    }
}

/// The door's site: the control-plane router on the shared serve floor.
struct ControlSite(multiview_control::ConnectionServer);

impl multiview_frontdoor::SiteService for ControlSite {
    fn serve(
        &self,
        io: multiview_frontdoor::DoorTlsStream,
        peer: SocketAddr,
        guard: multiview_frontdoor::ConnGuard,
    ) -> multiview_frontdoor::BoxFuture<()> {
        let server = self.0.clone();
        Box::pin(async move { server.serve(io, peer, guard).await })
    }
}

impl std::fmt::Debug for FrontDoorRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontDoorRun")
            .field("local_addr", &self.door.local_addr())
            .field("ice_tcp", &self.ice_tcp)
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
    ///
    /// Sockets passed by systemd ([`set_inherited_sockets`]) are used for the
    /// listeners whose type and port they match (UDP `https_port`, TCP door
    /// port, redirect port); the rest bind their own (slice 6, #292).
    #[must_use]
    pub fn start(config: &MultiviewConfig) -> Option<Self> {
        Self::start_with(config, take_inherited_sockets())
    }

    /// [`start`](Self::start) with explicit inherited sockets (tests).
    #[must_use]
    pub fn start_with(config: &MultiviewConfig, mut inherited: ListenSockets) -> Option<Self> {
        inherited.log_summary();
        warn_deprecated(config);
        let Some(fd_config) = config_from(config) else {
            inherited.report_unclaimed();
            return None;
        };
        let udp = inherited.take_udp_all(SocketAddr::new(
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            fd_config.port,
        ));
        let run = match multiview_frontdoor::FrontDoor::start_with(&fd_config, udp) {
            Ok(door) => {
                let mut run = Self {
                    door,
                    ice_tcp: false,
                };
                run.start_tcp(config, &mut inherited);
                Some(run)
            }
            Err(error) => {
                tracing::error!(%error, port = fd_config.port, "the front door failed to start; WebRTC and MoQ unavailable this run");
                None
            }
        };
        inherited.report_unclaimed();
        run
    }

    /// Start the TCP door and the redirect (slice 4). A bind failure is logged
    /// and the UDP door keeps running.
    fn start_tcp(&mut self, config: &MultiviewConfig, inherited: &mut ListenSockets) {
        let Some(plan) = tcp_config_from(config) else {
            return;
        };
        let listen = plan.door.listen;
        let ice_tcp = plan.door.ice_tcp;
        let tcp = match self.door.start_tcp_with(
            plan.door,
            multiview_frontdoor::TlsRouting::default(),
            None,
            inherited.take_tcp(listen),
        ) {
            Ok(local) => local,
            Err(error) => {
                tracing::error!(%error, %listen, "the front-door TCP door failed to start; the UDP door keeps running, TCP unavailable this run");
                return;
            }
        };
        let redirect = plan.redirect.and_then(|redirect| {
            let addr = redirect.listen;
            let socket = inherited.take_tcp(addr);
            match self.door.start_redirect_with(redirect, socket) {
                Ok(local) => Some(local),
                Err(error) => {
                    tracing::error!(%error, %addr, "the port-80 redirect failed to start; unavailable this run");
                    None
                }
            }
        });
        self.ice_tcp = ice_tcp;
        tracing::info!(
            %tcp,
            mode = ?plan.mode,
            redirect = ?redirect,
            ice_tcp,
            tls_site = false,
            "front-door TCP door listening (the site attaches once the control plane is built)"
        );
    }

    /// Whether the TCP door runs.
    #[must_use]
    pub fn has_tcp_door(&self) -> bool {
        self.door.tcp_local_addr().is_some()
    }

    /// Attach the site to the TCP door: `server` (the control-plane router on
    /// the same serve options as the plain listener, observing its shutdown)
    /// over `tls_site`. Without a TLS configuration the door keeps serving
    /// ICE-TCP only (a warning says so).
    pub fn install_site(
        &self,
        server: multiview_control::ConnectionServer,
        tls_site: Option<multiview_frontdoor::SiteTlsConfig>,
    ) {
        let Some(tcp) = self.door.tcp_local_addr() else {
            return;
        };
        let Some(site) = tls_site else {
            tracing::warn!(%tcp, "front-door TCP door: no site certificate ([network] cert_file/key_file or [control.tls] static, a `tls` build on unix), so TLS on the door is unavailable; ICE-TCP still runs");
            return;
        };
        // Read-modify-write: only the site half changes here, so a previously
        // installed ACME (TLS-ALPN-01) responder is never clobbered back to
        // `None` (#291).
        let service: Arc<dyn multiview_frontdoor::SiteService> = Arc::new(ControlSite(server));
        if self.door.set_site_config(Some(site)).is_ok()
            && self.door.set_site(Some(service)).is_ok()
        {
            tracing::info!(%tcp, tls_site = true, "front-door TCP door serves the site (HTTP/1.1 over the [control.tls] certificate)");
        }
    }

    /// Install (or replace) the ACME TLS-ALPN-01 challenge responder on the
    /// TCP door: only the ACME half of the routing changes, so a previously
    /// installed site certificate keeps serving. `multiview run` calls this
    /// once slice 5 PR B boots the `AcmeService` (#291); not called from this
    /// crate yet.
    #[cfg(all(feature = "tls", unix))]
    pub fn install_acme_resolver(
        &self,
        resolver: Arc<multiview_control::certificate_source::ChallengeResolver>,
    ) {
        let Some(tcp) = self.door.tcp_local_addr() else {
            return;
        };
        match multiview_control::acme_challenge_server_config(resolver) {
            Ok(config) => {
                if self.door.set_acme_config(Some(config)).is_ok() {
                    tracing::info!(%tcp, "front-door TCP door serves the ACME TLS-ALPN-01 challenge responder");
                }
            }
            Err(error) => {
                tracing::error!(%error, %tcp, "front-door TCP door: building the ACME challenge TLS configuration failed; acme-tls/1 hellos keep closing");
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

    /// Take the ICE-TCP lane (slice 4): `Some` only when the TCP door runs in
    /// `direct` mode with `network.ice_tcp.enabled`. Its
    /// [`local_port`](multiview_frontdoor::IceTcpLane::local_port) is the
    /// passive candidates' port.
    pub fn take_ice_tcp_lane(&mut self) -> Option<multiview_frontdoor::IceTcpLane> {
        if !self.ice_tcp {
            return None;
        }
        self.door.take_ice_tcp_lane().ok()
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
    fn mode_absent_means_no_tcp_door() {
        // mda001's saved state: https_port only. UDP door, no TCP listener.
        let config = load(&format!("{BASE}\n[network]\nhttps_port = 443\n"));
        assert!(config_from(&config).is_some());
        assert!(tcp_config_from(&config).is_none());
    }

    #[test]
    fn direct_mode_plans_the_door_and_the_redirect() {
        let config = load(&format!(
            "{BASE}\n[network]\nhttps_port = 443\ntcp_port = 8443\nmode = \"direct\"\npublic_host = \"mda001.example.net\"\n"
        ));
        let plan = tcp_config_from(&config).expect("direct");
        assert_eq!(plan.door.listen, "[::]:8443".parse().unwrap());
        assert!(plan.door.proxy.is_none());
        assert!(plan.door.ice_tcp);
        assert_eq!(
            plan.door.turn_sni.as_deref(),
            Some("turn.mda001.example.net")
        );
        let redirect = plan.redirect.expect("redirect");
        assert_eq!(redirect.listen, "[::]:80".parse().unwrap());
        assert_eq!(redirect.https_port, 8443);
        assert_eq!(redirect.public_host, "mda001.example.net");
        let no_redirect = load(&format!(
            "{BASE}\n[network]\nhttps_port = 443\nmode = \"direct\"\nhttp_redirect_port = 0\nbind = \"[::1]\"\n[network.ice_tcp]\nenabled = false\n"
        ));
        let plan = tcp_config_from(&no_redirect).unwrap();
        assert!(plan.redirect.is_none());
        assert!(!plan.door.ice_tcp);
        assert_eq!(plan.door.listen, "[::1]:443".parse().unwrap());
    }

    #[test]
    fn behind_proxy_plans_proxy_listen_and_trust() {
        let config = load(&format!(
            "{BASE}\n[network]\nhttps_port = 443\nmode = \"behind_proxy\"\ntrusted_proxies = [\"::1\", \"10.0.0.0/8\"]\n"
        ));
        let plan = tcp_config_from(&config).unwrap();
        assert_eq!(plan.door.listen, "[::1]:8443".parse().unwrap());
        assert!(plan.redirect.is_none());
        // Design note section 8: no ICE-TCP behind a proxy.
        assert!(!plan.door.ice_tcp);
        let trusted = plan.door.proxy.unwrap();
        assert!(trusted.contains("::1".parse().unwrap()));
        assert!(trusted.contains("10.9.8.7".parse().unwrap()));
        assert!(!trusted.contains("192.0.2.1".parse().unwrap()));
    }

    #[test]
    fn invalid_network_section_is_rejected_by_validation() {
        for bad in [
            "https_port = 0",
            "workers = 0",
            "bind = \"0.0.0.0\"",
            "https_port = 443\nmode = \"direct\"",
            "https_port = 443\nmode = \"behind_proxy\"",
        ] {
            let config = load(&format!("{BASE}\n[network]\n{bad}\n"));
            let network = config.network.as_ref().expect("[network] parsed");
            let err = network.validate().expect_err(bad).to_string();
            assert!(err.contains("network."), "{bad}: {err}");
        }
    }
}
