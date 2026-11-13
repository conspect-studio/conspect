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
//!
//! **Certificates (slice 5, ADR-0107 section 5a).** With `[network.tls]` the
//! run starts the ACME wiring ([`crate::tls_boot`]) before the control plane
//! binds, and:
//!
//! * the QUIC lane is held: MoQ starts on it only once the certificate source
//!   is `serving` ([`start_moq_when_serving`]), and then serves the ACME
//!   certificate through `SourceResolver` (renewals reach new handshakes with
//!   no restart), never the `MULTIVIEW_MOQ_CERTIFICATE` files;
//! * the TCP door (slice 4) serves the ACME certificate source as its site
//!   certificate (`[network.tls]` wins over `[control.tls]` for the door),
//!   answers TLS-ALPN-01 (ALPN exactly `acme-tls/1`) from the
//!   [`front_door_tls`] challenge resolver, and mounts
//!   `multiview_control::tls_setup::front_door_router` (the control router
//!   behind the setup gate). The `[control] listen` listener stays the
//!   loopback admin listener and is not gated.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

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

/// Install (or clear) the running front door's capability snapshot and
/// metrics source for the control plane, mirroring
/// `output_routing_control::install_program_metrics`: `main.rs` calls this
/// once, right after [`FrontDoorRun::start`] and before the front door
/// (which owns the data these borrow from) is moved into `RunningPersist`.
pub fn install(
    installed: Option<(
        multiview_control::system::NetworkCapability,
        Arc<dyn multiview_control::frontdoor_metrics::FrontDoorMetricsSource>,
    )>,
) {
    *slot().lock().unwrap_or_else(PoisonError::into_inner) = installed;
}

/// The installed capability snapshot, if a front door is running this
/// process.
#[must_use]
pub fn installed_capability() -> Option<multiview_control::system::NetworkCapability> {
    slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(cap, _)| cap.clone())
}

/// The installed metrics source, if a front door is running this process.
#[must_use]
pub fn installed_metrics_source(
) -> Option<Arc<dyn multiview_control::frontdoor_metrics::FrontDoorMetricsSource>> {
    slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, source)| Arc::clone(source))
}

type InstalledSlot = Mutex<
    Option<(
        multiview_control::system::NetworkCapability,
        Arc<dyn multiview_control::frontdoor_metrics::FrontDoorMetricsSource>,
    )>,
>;

fn slot() -> &'static InstalledSlot {
    static SLOT: OnceLock<InstalledSlot> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// The running front door and the lanes its consumers have not taken yet.
pub struct FrontDoorRun {
    door: multiview_frontdoor::FrontDoor,
    /// QUIC Retries sent (bumped by the vendored noq server through
    /// `moq_tokio::noq::Injected::retry_counter`; `0` until a `MoQ` listener
    /// is actually wired onto [`take_moq_socket`](Self::take_moq_socket)).
    retry_counter: Arc<AtomicU64>,
    /// `network.quic.retry_above_handshakes`, carried from config to the
    /// `MoQ` socket wiring. Only read by
    /// [`take_moq_socket`](Self::take_moq_socket) (`moq-output`); a build
    /// without that feature never reads it back, which is fine (it still
    /// reflects the configured value faithfully).
    #[cfg_attr(not(feature = "moq-output"), allow(dead_code))]
    retry_above_handshakes: usize,
    /// The live nft-floor status (re-read every
    /// [`crate::nft_floor::NFT_FLOOR_REREAD_INTERVAL`] on the front-door
    /// runtime, never the program runtime), plus the path to read.
    nft_floor: Arc<NftFloorCell>,
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
            .finish_non_exhaustive()
    }
}

/// A wait-free cell for the nft-floor status, re-read periodically on the
/// front-door runtime. 3 states fit `u8`; `Relaxed` is enough (telemetry,
/// not a correctness gate).
struct NftFloorCell(std::sync::atomic::AtomicU8);

impl NftFloorCell {
    fn new(initial: crate::nft_floor::NftFloorStatus) -> Self {
        Self(AtomicU8::new(encode(initial)))
    }

    fn store(&self, status: crate::nft_floor::NftFloorStatus) {
        self.0.store(encode(status), Ordering::Relaxed);
    }

    fn load(&self) -> crate::nft_floor::NftFloorStatus {
        decode(self.0.load(Ordering::Relaxed))
    }
}

const fn encode(status: crate::nft_floor::NftFloorStatus) -> u8 {
    match status {
        crate::nft_floor::NftFloorStatus::Present => 0,
        crate::nft_floor::NftFloorStatus::Absent => 1,
        crate::nft_floor::NftFloorStatus::Unknown => 2,
    }
}

const fn decode(value: u8) -> crate::nft_floor::NftFloorStatus {
    match value {
        0 => crate::nft_floor::NftFloorStatus::Present,
        1 => crate::nft_floor::NftFloorStatus::Absent,
        _ => crate::nft_floor::NftFloorStatus::Unknown,
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
    let udp = &network.udp;
    let max_quic = usize::try_from(network.quic.max_connections).unwrap_or(usize::MAX);
    Some(multiview_frontdoor::FrontDoorConfig {
        port,
        workers: usize::from(network.workers.max(1)),
        recv_buffer_bytes: usize::try_from(udp.recv_buffer_bytes).unwrap_or(usize::MAX),
        limits: multiview_frontdoor::NewFlowLimits {
            per_source_pps: udp.new_flow_pps_per_source,
            per_source_burst: udp.new_flow_burst,
            global_pps: udp.new_flow_global_pps,
            stun_per_source_pps: udp.stun_pps_per_source,
            // The config schema (section 10) exposes one stun burst knob
            // (`stun_pps_per_source`); the per-source burst mirrors it, same
            // as the crate default's own 1:1 ratio.
            stun_per_source_burst: udp.stun_pps_per_source,
            stun_global_pps: udp.stun_global_pps,
            ..multiview_frontdoor::NewFlowLimits::default()
        },
        max_rtc_peers: max_rtc,
        max_quic_peers: max_quic,
        turn_servers,
        steering: udp.validated_steering,
    })
}

/// `network.quic.retry_above_handshakes` and `network.udp.nft_floor_status_file`
/// are not part of [`multiview_frontdoor::FrontDoorConfig`] (they wire into
/// the `MoQ` listener and the nft-floor probe respectively, not the front
/// door itself); read them straight from `[network]` where needed.
#[must_use]
pub fn retry_above_handshakes(config: &MultiviewConfig) -> usize {
    config.network.as_ref().map_or(
        multiview_frontdoor::quic::DEFAULT_RETRY_ABOVE_HANDSHAKES,
        |n| n.quic.retry_above_handshakes,
    )
}

/// [`retry_above_handshakes`]'s sibling for the nft-floor status file path.
#[must_use]
pub fn nft_floor_status_file(config: &MultiviewConfig) -> std::path::PathBuf {
    config.network.as_ref().map_or_else(
        || std::path::PathBuf::from(multiview_config::network_tls::DEFAULT_NFT_FLOOR_STATUS_FILE),
        |n| n.udp.nft_floor_status_file.clone(),
    )
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
    ///
    /// Logs one line at start naming the steering mode, rcvbuf
    /// requested/granted/usable/clamped, kernel-drop support, and the nft
    /// floor (design note, "Slice 3 as built, part B"); starts the nft-floor re-reader on
    /// the front door's own runtime (never the program runtime).
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
                let nft_path = nft_floor_status_file(config);
                let initial_floor = crate::nft_floor::probe(&nft_path);
                let buffers = door.buffer_report();
                tracing::info!(
                    local = %door.local_addr(),
                    steering = ?door.steering(),
                    rcvbuf_requested = buffers.requested,
                    rcvbuf_granted = buffers.granted,
                    rcvbuf_usable = buffers.usable,
                    rcvbuf_clamped = buffers.clamped,
                    kernel_drops_supported = door.kernel_drops_supported(),
                    nft_floor = ?initial_floor,
                    "front door: started"
                );
                let nft_floor = Arc::new(NftFloorCell::new(initial_floor));
                door.handle()
                    .spawn(reread_nft_floor(nft_path, Arc::clone(&nft_floor)));
                let mut run = Self {
                    door,
                    retry_counter: Arc::new(AtomicU64::new(0)),
                    retry_above_handshakes: retry_above_handshakes(config),
                    nft_floor,
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
        // The site attaches once the control plane is built: hold site hellos
        // that arrive before that (bounded by the handshake deadline) instead
        // of closing them; `install_site` releases them.
        // (No `[control]`: no control plane, no site; nothing is held.)
        if config.control.is_some() {
            let _ = self.door.set_site_pending(true);
        }
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
    /// over `tls_site`, and `acme` (the TLS-ALPN-01 responder, ALPN exactly
    /// `acme-tls/1`) when `[network.tls]` is configured (slice 5). Without a
    /// site TLS configuration the door keeps serving ICE-TCP only (a warning
    /// says so).
    pub fn install_site(
        &self,
        server: multiview_control::ConnectionServer,
        tls_site: Option<multiview_frontdoor::SiteTlsConfig>,
        acme: Option<multiview_frontdoor::SiteTlsConfig>,
    ) {
        let Some(tcp) = self.door.tcp_local_addr() else {
            return;
        };
        let Some(site) = tls_site else {
            let _ = self.door.set_site_pending(false);
            tracing::warn!(%tcp, "front-door TCP door: no site certificate ([network.tls] in an `acme` build, or [network] cert_file/key_file or [control.tls] static in a `tls` build, on unix), so TLS on the door is unavailable; ICE-TCP still runs");
            return;
        };
        // Read-modify-write per half (#291): the site half here; the ACME
        // (TLS-ALPN-01) half through `set_acme_config`, so neither clobbers
        // the other.
        let challenge = acme.is_some();
        let service: Arc<dyn multiview_frontdoor::SiteService> = Arc::new(ControlSite(server));
        if let Some(acme) = acme {
            if self.door.set_acme_config(Some(acme)).is_err() {
                tracing::error!(%tcp, "front-door TCP door: installing the ACME TLS-ALPN-01 responder failed");
            }
        }
        if self.door.set_site_config(Some(site)).is_ok()
            && self.door.set_site(Some(service)).is_ok()
        {
            if challenge {
                tracing::info!(%tcp, tls_site = true, tls_alpn_01 = true, "front-door TCP door serves the site (HTTP/1.1 over the ACME certificate, behind the setup gate) and answers TLS-ALPN-01");
            } else {
                tracing::info!(%tcp, tls_site = true, "front-door TCP door serves the site (HTTP/1.1 over the [control.tls] certificate)");
            }
        }
    }

    /// The bound TCP door address, if the door runs.
    #[must_use]
    pub fn tcp_local_addr(&self) -> Option<std::net::SocketAddr> {
        self.door.tcp_local_addr()
    }

    /// Install (or replace) the ACME TLS-ALPN-01 challenge responder on the
    /// TCP door: only the ACME half of the routing changes, so a previously
    /// installed site certificate keeps serving. With `[network.tls]` the site
    /// install path ([`Self::install_site`]) passes the same responder.
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

    /// Take the QUIC lane, wrapped for the `MoQ` listener. Carries
    /// `network.quic.retry_above_handshakes` and the retry counter this run
    /// owns (read back by [`stats`](Self::stats_report) and the capability
    /// snapshot).
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
            retry_above_handshakes: Some(self.retry_above_handshakes),
            retry_counter: Some(Arc::clone(&self.retry_counter)),
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

    /// The startup capability snapshot (`GET /api/v1/system/capabilities`,
    /// design note "Slice 3 as built, part B"): steering mode, rcvbuf report, kernel-drop
    /// support, and the nft floor **as read at startup** (the live value is
    /// `/api/v1/metrics/frontdoor`).
    #[must_use]
    pub fn capability(&self) -> multiview_control::system::NetworkCapability {
        network_capability(&self.door, self.nft_floor.load())
    }

    /// A [`multiview_control::frontdoor_metrics::FrontDoorMetricsSource`]
    /// over this run's live counters, steering mode and nft floor.
    #[must_use]
    pub fn metrics_source(
        &self,
    ) -> Arc<dyn multiview_control::frontdoor_metrics::FrontDoorMetricsSource> {
        Arc::new(LiveFrontDoorMetrics {
            stats: self.door.stats(),
            mode: self.door.steering(),
            retry_counter: Arc::clone(&self.retry_counter),
            nft_floor: Arc::clone(&self.nft_floor),
        })
    }
}

/// Map the door's steering mode + a resolved nft-floor status onto the
/// capability DTO (shared by [`FrontDoorRun::capability`] and
/// [`LiveFrontDoorMetrics`]).
fn network_capability(
    door: &multiview_frontdoor::FrontDoor,
    nft_floor: crate::nft_floor::NftFloorStatus,
) -> multiview_control::system::NetworkCapability {
    use multiview_control::system::{NftFloorStatus, RecvBufferCapability, Udp443Steering};
    let buffers = door.buffer_report();
    let (udp443_steering, steering_reason) = match door.steering() {
        multiview_frontdoor::SteeringMode::ReuseportCbpf => (Udp443Steering::ReuseportCbpf, None),
        multiview_frontdoor::SteeringMode::SingleSocket { reason } => {
            (Udp443Steering::SingleSocket, Some(reason))
        }
    };
    multiview_control::system::NetworkCapability {
        udp443_steering,
        steering_reason,
        recv_buffer: RecvBufferCapability {
            requested: u64::try_from(buffers.requested).unwrap_or(u64::MAX),
            granted: u64::try_from(buffers.granted).unwrap_or(u64::MAX),
            usable: u64::try_from(buffers.usable).unwrap_or(u64::MAX),
            clamped: buffers.clamped,
        },
        kernel_drops_supported: door.kernel_drops_supported(),
        nft_floor: match nft_floor {
            crate::nft_floor::NftFloorStatus::Present => NftFloorStatus::Present,
            crate::nft_floor::NftFloorStatus::Absent => NftFloorStatus::Absent,
            crate::nft_floor::NftFloorStatus::Unknown => NftFloorStatus::Unknown,
        },
    }
}

/// [`FrontDoorRun::metrics_source`]'s implementation: every field is a
/// wait-free atomic load (invariant #10), never an engine channel.
struct LiveFrontDoorMetrics {
    stats: Arc<multiview_frontdoor::Stats>,
    mode: multiview_frontdoor::SteeringMode,
    retry_counter: Arc<AtomicU64>,
    nft_floor: Arc<NftFloorCell>,
}

impl multiview_control::frontdoor_metrics::FrontDoorMetricsSource for LiveFrontDoorMetrics {
    fn read(&self) -> Option<multiview_control::frontdoor_metrics::FrontDoorMetricsReport> {
        use multiview_control::system::{NftFloorStatus, Udp443Steering};
        let s = self.stats.snapshot();
        let udp443_steering = match self.mode {
            multiview_frontdoor::SteeringMode::ReuseportCbpf => Udp443Steering::ReuseportCbpf,
            multiview_frontdoor::SteeringMode::SingleSocket { .. } => Udp443Steering::SingleSocket,
        };
        let nft_floor = match self.nft_floor.load() {
            crate::nft_floor::NftFloorStatus::Present => NftFloorStatus::Present,
            crate::nft_floor::NftFloorStatus::Absent => NftFloorStatus::Absent,
            crate::nft_floor::NftFloorStatus::Unknown => NftFloorStatus::Unknown,
        };
        Some(
            multiview_control::frontdoor_metrics::FrontDoorMetricsReport {
                received: s.received,
                to_quic: s.to_quic,
                to_rtc: s.to_rtc,
                dropped_classify: s.dropped_classify,
                dropped_per_source: s.dropped_per_source,
                dropped_global: s.dropped_global,
                dropped_vn: s.dropped_vn,
                dropped_initial: s.dropped_initial,
                dropped_queue_full: s.dropped_queue_full,
                dropped_no_consumer: s.dropped_no_consumer,
                recv_errors: s.recv_errors,
                tracked_sources: s.tracked_sources,
                received_established_socket: s.received_established_socket,
                received_listener_socket: s.received_listener_socket,
                steered_unverified: s.steered_unverified,
                steering_overflow: s.steering_overflow,
                steering_updates: s.steering_updates,
                steering_update_errors: s.steering_update_errors,
                steered_peers: s.steered_peers,
                kernel_drops_listener: s.kernel_drops_listener,
                kernel_drops_established: s.kernel_drops_established,
                dropped_stun_per_source: s.dropped_stun_per_source,
                dropped_stun_global: s.dropped_stun_global,
                dropped_established_peer: s.dropped_established_peer,
                quic_retries: self.retry_counter.load(Ordering::Relaxed),
                udp443_steering,
                nft_floor,
            },
        )
    }
}

/// Re-read the nft-floor status file every
/// [`crate::nft_floor::NFT_FLOOR_REREAD_INTERVAL`] into `cell`. Runs on the
/// front-door runtime (never the program runtime, invariant #1): a plain
/// file read every 10 minutes is cheap and never blocks a data-plane path.
async fn reread_nft_floor(path: std::path::PathBuf, cell: Arc<NftFloorCell>) {
    let mut tick = tokio::time::interval(crate::nft_floor::NFT_FLOOR_REREAD_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; the caller already probed once at
    // startup, so skip it to avoid a redundant duplicate read.
    tick.tick().await;
    loop {
        tick.tick().await;
        let path = path.clone();
        if let Ok(status) =
            tokio::task::spawn_blocking(move || crate::nft_floor::probe(&path)).await
        {
            cell.store(status);
        }
    }
}

/// The run's TLS-ALPN-01 challenge resolver (ACME certificate, with
/// challenges answered for ALPN `acme-tls/1`), when the run has
/// `[network.tls]`. The TCP door's `acme-tls/1` route serves it
/// (`multiview_control::acme_challenge_server_config`).
#[cfg(all(feature = "acme", unix))]
#[must_use]
pub fn front_door_tls() -> Option<Arc<multiview_control::certificate_source::ChallengeResolver>> {
    crate::tls_boot::installed_tls_context().map(|tls| Arc::clone(&tls.resolver))
}

/// Hold the front-door QUIC lane until the ACME source is serving, then start
/// `MoQ` on it with the ACME certificate and install the real provider into
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
    if let Ok(provider) = multiview_control::moq_startup::start_shared_resolver(
        endpoint, protocol, factory, socket, resolver,
    )
    .await
    {
        deferred.set(provider);
        tracing::info!("front door: certificate serving; QUIC/MoQ started");
    } else {
        tracing::error!("front door: MoQ listener failed to start on the QUIC lane");
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
