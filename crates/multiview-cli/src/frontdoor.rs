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
    /// The HTTP/3 endpoint on the QUIC lane (slice 2).
    h3: H3Slot,
}

/// `MoQ` over the h3 endpoint, installed by the run before the control plane
/// binds ([`FrontDoorRun::set_h3_moq`]).
pub struct H3Moq {
    /// The WebTransport handler (`MoqWt`).
    pub wt: Arc<dyn multiview_h3::WtHandler>,
    /// Called once the endpoint is up (installs the real `MoQ` provider, so
    /// leases are issued only while the endpoint serves them).
    pub on_up: Box<dyn FnOnce() + Send>,
    /// The endpoint certificate when no site certificate is configured (the
    /// `MULTIVIEW_MOQ_CERTIFICATE` pair).
    pub fallback_tls: Option<multiview_frontdoor::SiteTlsConfig>,
}

impl std::fmt::Debug for H3Moq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Moq")
            .field("fallback_tls", &self.fallback_tls.is_some())
            .finish_non_exhaustive()
    }
}

/// The certificate the h3 endpoint serves, and when it may start.
pub struct H3Tls {
    /// The site TLS configuration (ALPN is overridden to `h3`, 0-RTT off).
    /// `None`: the `MoQ` fallback pair, else no endpoint.
    pub config: Option<multiview_frontdoor::SiteTlsConfig>,
    /// With `[network.tls]`: resolves `true` once the certificate is
    /// `serving` (`false`: never, e.g. shutdown). No QUIC in `setup`.
    pub ready: Option<std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>>,
    /// Why this certificate (for the startup log).
    pub source: &'static str,
}

impl std::fmt::Debug for H3Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Tls")
            .field("config", &self.config.is_some())
            .field("deferred", &self.ready.is_some())
            .field("source", &self.source)
            .finish()
    }
}

/// The h3 endpoint's state inside the run.
struct H3Slot {
    lane: Mutex<Option<multiview_frontdoor::QuicLane>>,
    moq: Mutex<Option<H3Moq>>,
    alt_svc: Arc<multiview_control::h3_site::AltSvc>,
    /// `[network] alt_svc` (the operator's "UDP reachable").
    alt_svc_allowed: bool,
    endpoint: Arc<Mutex<Option<multiview_h3::H3Endpoint>>>,
    starter: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// `behind_proxy` mode only: refuses a QUIC source that is a trusted
    /// proxy (see [`refuse_source_from`]). `None` in `direct` mode or with
    /// no `network.mode`.
    refuse_source: Option<multiview_h3::SourceFilter>,
}

/// Builds the h3 [`multiview_h3::SourceFilter`] for `behind_proxy` mode, or
/// `None` in `direct` mode / with no `network.mode`.
///
/// Reason (design note §8): a plain UDP forward in `behind_proxy` mode
/// delivers every QUIC datagram with the forwarder's own source address, so
/// a QUIC connection whose source is a `trusted_proxies` address is a
/// forwarder hop that has already lost the real client's address — QUIC has
/// no PROXY header to recover it from, unlike the TCP door. That connection
/// is refused, failing closed the same way the TCP door fails closed on a
/// missing/untrusted PROXY v2 header. A source-preserving forward (DNAT,
/// design note §8) delivers the real client address instead of the
/// forwarder's, so it is not in `trusted_proxies` and is served normally,
/// with `Alt-Svc` advertised as §8 describes.
fn refuse_source_from(config: &MultiviewConfig) -> Option<multiview_h3::SourceFilter> {
    let network = config.network.as_ref()?;
    match network.mode? {
        NetworkMode::BehindProxy => {
            let trusted = network.trusted_proxy_list().ok()?;
            let proxies = multiview_frontdoor::TrustedProxies::new(
                trusted.iter().map(|t| (t.addr, t.prefix)),
            );
            let filter: multiview_h3::SourceFilter =
                Arc::new(move |addr: SocketAddr| proxies.contains(addr.ip()));
            Some(filter)
        }
        NetworkMode::Direct => None,
    }
}

/// Locks, recovering from poisoning (the guarded values stay consistent:
/// every critical section is a single take/insert).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// Upper bound on the h3 endpoint's shutdown (its own drain is 2 s).
const H3_SHUTDOWN_BOUND: std::time::Duration = std::time::Duration::from_secs(3);

impl std::fmt::Debug for FrontDoorRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontDoorRun")
            .field("local_addr", &self.door.local_addr())
            .field("ice_tcp", &self.ice_tcp)
            .field("h3_running", &self.h3_running())
            .finish_non_exhaustive()
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
            Ok(mut door) => {
                let lane = door.take_quic_lane().ok();
                let port = door.local_addr().port();
                let alt_svc_allowed = config
                    .network
                    .as_ref()
                    .is_none_or(multiview_config::NetworkConfig::alt_svc_enabled);
                let refuse_source = refuse_source_from(config);
                let mut run = Self {
                    door,
                    ice_tcp: false,
                    h3: H3Slot {
                        lane: Mutex::new(lane),
                        moq: Mutex::new(None),
                        alt_svc: Arc::new(multiview_control::h3_site::AltSvc::new(port)),
                        alt_svc_allowed,
                        endpoint: Arc::new(Mutex::new(None)),
                        starter: Mutex::new(None),
                        refuse_source,
                    },
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

    /// Install `MoQ` on the h3 endpoint (before [`start_h3`](Self::start_h3)).
    pub fn set_h3_moq(&self, moq: H3Moq) {
        *lock(&self.h3.moq) = Some(moq);
    }

    /// The `Alt-Svc` switch the TCP door's site layer reads.
    #[must_use]
    pub fn alt_svc(&self) -> Arc<multiview_control::h3_site::AltSvc> {
        Arc::clone(&self.h3.alt_svc)
    }

    /// Whether the h3 endpoint is serving.
    #[must_use]
    pub fn h3_running(&self) -> bool {
        lock(&self.h3.endpoint).is_some()
    }

    /// The h3 endpoint's counters, while it serves (telemetry, tests).
    #[must_use]
    pub fn h3_counters(&self) -> Option<Arc<multiview_h3::Counters>> {
        lock(&self.h3.endpoint)
            .as_ref()
            .map(|e| Arc::clone(e.counters()))
    }

    /// The h3 endpoint's local address, while it serves.
    #[must_use]
    pub fn h3_local_addr(&self) -> Option<SocketAddr> {
        lock(&self.h3.endpoint)
            .as_ref()
            .and_then(|e| e.local_addr().ok())
    }

    /// Start the HTTP/3 endpoint on the QUIC lane (slice 2, design note
    /// section 5): `site` answers every request, `MoQ` ([`set_h3_moq`](Self::set_h3_moq))
    /// every WebTransport CONNECT under `/moq/`. Everything (the noq driver,
    /// the accept loop, each connection, this starter) runs on the front
    /// door's runtime. With `tls.ready` the endpoint starts only once it
    /// resolves `true` (ACME: `serving`); `Alt-Svc` turns on once the endpoint
    /// is up, unless `[network] alt_svc = false`. Without any certificate no
    /// endpoint starts (a warning says so) and `MoQ` stays unavailable.
    /// A second call does nothing.
    pub fn start_h3(&self, site: Arc<dyn multiview_h3::Site>, tls: H3Tls) {
        let Some(lane) = lock(&self.h3.lane).take() else {
            return;
        };
        let moq = lock(&self.h3.moq).take();
        let (wt, on_up, fallback) = match moq {
            Some(m) => (Some(m.wt), Some(m.on_up), m.fallback_tls),
            None => (None, None, None),
        };
        let (config, source) = match (tls.config, fallback) {
            (Some(config), _) => (config, tls.source),
            (None, Some(config)) => (config, "MULTIVIEW_MOQ_CERTIFICATE"),
            (None, None) => {
                tracing::warn!(
                    udp = %self.door.local_addr(),
                    "front door: no certificate for the HTTP/3 endpoint ([network.tls], \
                     [network] cert_file/key_file or [control.tls] in a `tls` build, or the \
                     MULTIVIEW_MOQ_CERTIFICATE pair); QUIC is not served, the site stays on \
                     TCP, MoQ is unavailable, no Alt-Svc"
                );
                return;
            }
        };
        let established = self.door.quic_established();
        let peers: multiview_h3::PeersHook = Arc::new(move |peer, up| {
            if up {
                established.insert(peer);
            } else {
                established.remove(peer);
            }
        });
        let runtime = self.door.handle();
        if self.h3.refuse_source.is_some() {
            tracing::info!(
                "behind_proxy: QUIC from trusted_proxies is refused (forwarder hop, client \
                 address lost); forward UDP with DNAT that preserves source addresses"
            );
        }
        let parts = multiview_h3::EndpointParts {
            socket: Box::new(lane),
            endpoint: multiview_frontdoor::quic::endpoint_config(),
            incoming: multiview_frontdoor::quic::incoming_caps(),
            tls: config,
            runtime: runtime.clone(),
            site,
            wt,
            limits: multiview_h3::H3Limits::default(),
            peers: Some(peers),
            refuse_source: self.h3.refuse_source.clone(),
        };
        let slot = Arc::clone(&self.h3.endpoint);
        let alt_svc = Arc::clone(&self.h3.alt_svc);
        let alt_allowed = self.h3.alt_svc_allowed;
        let ready = tls.ready;
        let starter = runtime.spawn(async move {
            if let Some(ready) = ready {
                tracing::info!("front door: QUIC/HTTP/3 held until the ACME certificate is serving");
                if !ready.await {
                    return;
                }
            }
            let moq = parts.wt.is_some();
            match multiview_h3::H3Endpoint::start(parts).await {
                Ok(endpoint) => {
                    let udp = endpoint.local_addr().ok();
                    *lock(&slot) = Some(endpoint);
                    if let Some(on_up) = on_up {
                        on_up();
                    }
                    alt_svc.set(alt_allowed);
                    tracing::info!(
                        udp = ?udp,
                        alpn = "h3",
                        zero_rtt = false,
                        moq,
                        certificate = source,
                        alt_svc = alt_allowed,
                        alt_svc_why = if alt_allowed {
                            "endpoint up, [network] alt_svc on"
                        } else {
                            "[network] alt_svc = false (UDP declared unreachable)"
                        },
                        "front door: HTTP/3 endpoint up (site over h3, MoQ over WebTransport under /moq/)"
                    );
                }
                Err(error) => {
                    tracing::error!(%error, "front door: the HTTP/3 endpoint failed to start; the site stays on TCP, MoQ unavailable, no Alt-Svc");
                }
            }
        });
        *lock(&self.h3.starter) = Some(starter);
    }

    /// Shut the front door down: its runtime (dispatcher, QUIC and WebRTC
    /// tasks) stops and the socket is closed, waiting at most
    /// [`multiview_frontdoor::SHUTDOWN_TIMEOUT`] on a blocking-pool thread.
    pub async fn shutdown(self) {
        // The h3 endpoint first (Alt-Svc off, connections closed, bounded by
        // its shutdown timeout), then the runtime it runs on.
        self.h3.alt_svc.set(false);
        if let Some(starter) = lock(&self.h3.starter).take() {
            starter.abort();
        }
        let endpoint = lock(&self.h3.endpoint).take();
        if let Some(endpoint) = endpoint {
            if tokio::time::timeout(H3_SHUTDOWN_BOUND, endpoint.shutdown())
                .await
                .is_err()
            {
                tracing::warn!("front door: the HTTP/3 endpoint did not drain in time");
            }
        }
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

/// The run's TLS-ALPN-01 challenge resolver (ACME certificate, with
/// challenges answered for ALPN `acme-tls/1`), when the run has
/// `[network.tls]`. The TCP door's `acme-tls/1` route serves it
/// (`multiview_control::acme_challenge_server_config`).
#[cfg(all(feature = "acme", unix))]
#[must_use]
pub fn front_door_tls() -> Option<Arc<multiview_control::certificate_source::ChallengeResolver>> {
    crate::tls_boot::installed_tls_context().map(|tls| Arc::clone(&tls.resolver))
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
