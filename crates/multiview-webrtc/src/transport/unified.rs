//! The single-socket [`UnifiedEndpoint`] (ADR-0048 §4) — **feature `native`**.
//!
//! ADR-0048 mandates **one** process-wide dual-stack UDP socket adopted by **all**
//! WebRTC roles: WHIP ingest publishers, WHEP preview viewers, WHEP output
//! viewers, and the outbound `whip_push` client. Before this, the cli bound the
//! `webrtc.udp_port` once per role (preview + WHIP + WHEP-serve + each `whip_push`),
//! so the 2nd/3rd `bind` hit `EADDRINUSE` and silently degraded those roles to
//! "unavailable" — with preview + WHIP + WHEP-serve in one config, ingest and
//! output-serve were dead (box-validation defect B).
//!
//! [`UnifiedEndpoint`] fixes that: it binds the **one** socket, owns the **one**
//! [`TurnRelayDriver`] (so every role shares the relay allocations), and runs a
//! **single** driver task that demultiplexes every inbound datagram to the right
//! role's session by str0m's ufrag/peer demux (`Rtc::accepts()`) — exactly the
//! sans-IO single-socket pattern. Outbound datagrams are routed relay-aware (a
//! str0m `Transmit` whose source is an allocated relay is framed as a TURN Send
//! indication — defect C). No `SO_REUSEPORT`, one socket, one driver, many
//! sessions.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The driver never `.await`s a peer (UDP send is non-blocking); per-role media
//! crosses only bounded drop-oldest rings (ingest `RtpRing`, egress `SampleFeed` /
//! `EgressFeed`). A wedged or saturated endpoint loses *preview/ingest/viewer
//! media*, never an output tick. Registration crosses bounded command channels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::EndpointConfig;
use crate::egress::EgressFeed;
use crate::error::{Result, WebRtcError};
use crate::transport::whep_serve::{ServeLane, WhepServeHandle};
use crate::transport::whip_endpoint::{IngestLane, WhipEndpoint, WhipHandle};
use crate::transport::whip_push::{PushLane, WhipPushSpec};
use crate::transport::{Transport, WebRtcEndpoint, WhepServeEndpoint};
use crate::turn::TurnRelayDriver;

/// The maximum UDP datagram the unified recv loop reads.
const RECV_BUFFER: usize = 2048;

/// How often the driver wakes to advance timers, drain feeds, GC, and pump TURN
/// when otherwise idle. The tightest role cadence (WHEP egress) wins so program
/// AUs are packetized promptly.
const DRIVER_TICK: Duration = Duration::from_millis(10);

/// Per-wake inbound drain budget: the **maximum datagrams** one `readable` wake
/// reads from the socket before it MUST break to pump str0m timers / TURN
/// keepalives / commands / GC and yield back to `select!` (invariant #10). At the
/// ADR-0048 §12 worst case (~50 kpps aggregate), 256 datagrams is ~5 ms of
/// arrivals — bounded work — while the `select!` re-fires immediately if the
/// socket still has data, so no datagram is *lost* by the cap (only deferred to
/// the next wake; the OS receive buffer holds the rest, dropping oldest on
/// overflow exactly as UDP already does). Without this cap a sustained/hostile
/// flood (the socket never returns `WouldBlock`) would keep the task in the inner
/// loop forever and starve the whole driver — the panel's MAJOR #1.
const MAX_DATAGRAMS_PER_WAKE: usize = 256;

/// Per-wake inbound drain **time budget**: even below [`MAX_DATAGRAMS_PER_WAKE`],
/// once this much wall-clock has elapsed inside one drain the loop breaks to
/// pump/yield. A belt-and-braces bound for the case where per-datagram routing is
/// unexpectedly costly, so a wake can never monopolise the task for longer than
/// this regardless of datagram size/rate. Well under [`DRIVER_TICK`] so pumping
/// and the tick arm still run promptly.
const MAX_DRAIN_TIME: Duration = Duration::from_millis(2);

/// How a single-wake inbound drain ended (invariant #10, MAJOR #1).
/// Which `select!` arm triggered a [`UnifiedEndpoint::service_timers_and_gc`]
/// pass, so the function itself — not a separate call site — can distinguish
/// the idle-tick servicing cadence from the invariant-#10 bounded handoff that
/// must fire once per exhausted receive budget. Keeping the distinction inside
/// the function (rather than an increment the caller bolts on afterwards)
/// means a regression that stops *calling* the handoff pass can never still
/// satisfy the handoff counter: there is no second place to fake it.
///
/// `cfg(test)`-only (review PR193-04): this origin tag, the parameter that
/// threads it, and the [`RunProbe::handoff_services`] counter it gates exist
/// solely so a soak test can assert the handoff fires once per exhausted
/// budget. None of it is needed for the production servicing pass itself
/// (which always runs the same work regardless of why it was invoked), so it
/// must not cost a release build an extra enum/branch/atomic.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceOrigin {
    /// The periodic idle-tick arm (`tick.tick()`).
    IdleTick,
    /// The explicit bounded handoff run immediately after a receive budget
    /// was exhausted (invariant #10, round-2 BLOCKER).
    ExhaustionHandoff,
    /// The outbound-only pump after a wake that drained the socket to
    /// `WouldBlock` (not a full servicing pass; tags sends only).
    DrainedDry,
}

/// Most STUN Binding Requests [`RunProbe::sent_checks`] retains, so the
/// test-only send log stays bounded however long a run lasts.
#[cfg(test)]
const SENT_CHECK_LOG_CAP: usize = 256;

/// Whether `payload` is a STUN Binding Request (type 0x0001 + magic cookie).
#[cfg(test)]
fn is_stun_binding_request(payload: &[u8]) -> bool {
    payload.len() >= 20 && payload[..2] == [0x00, 0x01] && payload[4..8] == [0x21, 0x12, 0xA4, 0x42]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DrainOutcome {
    /// Datagrams read this wake (routed + dropped-malformed), capped by the budget.
    read_count: usize,
    /// `true` when the per-wake budget (count or time) stopped the drain before
    /// the socket reported `WouldBlock` — the socket may still hold data, so the
    /// caller must re-arm the `select!` (after pumping/yielding).
    budget_exhausted: bool,
}

/// Diagnostic counters for the driver loop's invariant-#10 handoff, recorded
/// unconditionally (two relaxed adds, only when each event fires) so a soak test
/// can assert the **ordering** guarantee the round-2 BLOCKER turned on: every
/// exhausted receive budget is followed by a deterministic servicing pump
/// **before** the next datagram is read. Production calls
/// [`UnifiedEndpoint::run`], which threads a throwaway probe; only tests inspect
/// the counts.
#[derive(Debug, Default)]
#[doc(hidden)]
struct RunProbe {
    /// Times an inbound drain stopped on its per-wake budget (still-readable
    /// socket) rather than `WouldBlock`.
    recv_budgets_exhausted: std::sync::atomic::AtomicU64,
    /// Times the driver ran a full servicing pump (commands, timers, outbound,
    /// GC) — once per idle tick, and once per exhausted receive budget as the
    /// bounded handoff.
    driver_services: std::sync::atomic::AtomicU64,
    /// Times the driver ran the explicit **bounded handoff** servicing pump
    /// specifically — i.e. the pump triggered by an exhausted receive budget,
    /// distinct from the pump triggered by an idle tick. Unlike
    /// [`driver_services`](Self::driver_services) (bumped by both paths), this
    /// counter is bumped ONLY on the exhaustion handoff path, so a test can
    /// assert the handoff itself keeps pace with exhaustions — an idle tick
    /// that happens to also run a service cannot inflate this counter and mask
    /// a regression that drops the explicit handoff.
    ///
    /// `cfg(test)`-only (review PR193-04): nothing in production distinguishes
    /// the handoff pass from an idle-tick pass (both run identical work), so
    /// this counter — and the [`ServiceOrigin`] tag that feeds it — would be
    /// pure unused instrumentation state in a release build.
    #[cfg(test)]
    handoff_services: std::sync::atomic::AtomicU64,
    /// Completed idle-tick servicing passes (`ServiceOrigin::IdleTick`).
    /// `cfg(test)`-only: the shared-lane fairness test asserts the tick keeps
    /// running under a permanently ready lane.
    #[cfg(test)]
    idle_tick_services: std::sync::atomic::AtomicU64,
    /// Times a readable wake drained the socket to `WouldBlock` (or failed)
    /// instead of exhausting its budget — a NATURAL yield point, after which
    /// the next `readable()` can return `Pending`. `cfg(test)`-only (review
    /// PR193-06): the flood test uses it to prove the socket stayed
    /// continuously readable across each fairness interval it measures, so a
    /// natural drain-to-empty cannot stand in for the production `yield_now`.
    #[cfg(test)]
    drained_dry: std::sync::atomic::AtomicU64,
    /// Consecutive exhausted budgets since the last dry drain (reset to 0 by
    /// one), and the longest such run seen. During a dry-free run the receive
    /// arm never returns `Pending`, so the ONLY way another task on the
    /// runtime can be scheduled inside it is the production `yield_now`
    /// (review PR193-06). `cfg(test)`-only.
    #[cfg(test)]
    dry_free_run: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    max_dry_free_run: std::sync::atomic::AtomicU64,
    /// Every STUN Binding Request the driver sent, recorded at the actual
    /// send site with the servicing path that sent it, its destination and
    /// its exact bytes (bounded by [`SENT_CHECK_LOG_CAP`]). The flood test
    /// matches a datagram the peer received against this log, so the pump
    /// effect is attributed at its source rather than inferred from a time
    /// window (review PR193-07). `cfg(test)`-only.
    #[cfg(test)]
    sent_checks: std::sync::Mutex<Vec<(ServiceOrigin, SocketAddr, Vec<u8>)>>,
}

#[cfg(test)]
impl RunProbe {
    /// Record one sent datagram if it is a STUN Binding Request.
    fn record_send(&self, origin: ServiceOrigin, dst: SocketAddr, payload: &[u8]) {
        if !is_stun_binding_request(payload) {
            return;
        }
        if let Ok(mut log) = self.sent_checks.lock() {
            if log.len() < SENT_CHECK_LOG_CAP {
                log.push((origin, dst, payload.to_vec()));
            }
        }
    }
}

/// An IPv4-mapped IPv6 socket address as plain IPv4.
fn canonical_peer(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => v6
            .ip()
            .to_ipv4_mapped()
            .map_or(addr, |v4| SocketAddr::new(v4.into(), v6.port())),
        SocketAddr::V4(_) => addr,
    }
}

/// One datagram delivered by a shared front door (ADR-0107 slice 1).
#[derive(Debug, Clone)]
pub struct InboundDatagram {
    /// The remote address, as the dual-stack socket reported it.
    pub src: SocketAddr,
    /// The concrete local IP the datagram arrived on (PKTINFO), IPv4-mapped
    /// addresses unmapped; `None` when the kernel reported none.
    pub dst_ip: Option<std::net::IpAddr>,
    /// The datagram.
    pub payload: Vec<u8>,
}

/// The receive side of a socket the endpoint shares with other protocols (the
/// front door's RFC 9443 dispatcher, ADR-0107). The endpoint only *sends* on
/// its own descriptor; every datagram arrives through this, already classified
/// as STUN/DTLS/RTP/TURN and rate limited.
pub trait SharedInbound: Send + 'static {
    /// Poll the next datagram; `Ready(None)` once the source is gone.
    fn poll_next(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<InboundDatagram>>;
    /// Take a queued datagram without waiting.
    fn try_next(&mut self) -> Option<InboundDatagram>;
    /// A session's remote address passed ICE (STUN MESSAGE-INTEGRITY) and DTLS:
    /// the front door may put it on the established path. Returns whether it
    /// was accepted (the set is capped).
    fn validated(&mut self, peer: SocketAddr) -> bool;
    /// A previously validated address is no longer used by any session.
    fn invalidated(&mut self, peer: SocketAddr);
}

/// One deframed ICE-TCP frame delivered by a shared front door's TCP door
/// (ADR-0107 slice 4, RFC 6544 / RFC 4571).
#[derive(Debug, Clone)]
pub struct InboundFrame {
    /// The remote peer (IPv4-mapped addresses unmapped).
    pub peer: SocketAddr,
    /// The accepted connection's local address (the TCP door's address).
    pub local: SocketAddr,
    /// The frame payload, without the RFC 4571 length prefix.
    pub payload: Vec<u8>,
}

/// The send half of an ICE-TCP lane, usable as `&dyn TcpSend` by every pump.
pub trait TcpSend: Send + Sync {
    /// Never blocks: queue the frame on the peer's connection (drop-oldest
    /// inside). `false` = no such connection.
    fn send(&self, peer: SocketAddr, payload: &[u8]) -> bool;
}

/// The ICE-TCP lane of a front door's TCP door: deframed inbound frames from
/// every ICE-TCP connection, and a non-blocking send (via [`TcpSend`]).
pub trait SharedTcp: TcpSend + 'static {
    /// Poll the next frame; `Ready(None)` once the door is gone.
    fn poll_next(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<InboundFrame>>;
    /// Take a queued frame without waiting.
    fn try_next(&mut self) -> Option<InboundFrame>;
}

/// Where a pump sends: the UDP socket, plus the ICE-TCP lane when the endpoint
/// runs on a front door with ICE-TCP. A [`Transport::Tcp`] datagram goes to
/// `tcp` (dropped when absent), never to the socket.
#[derive(Clone, Copy)]
pub struct Egress<'a> {
    /// The UDP send socket.
    pub socket: &'a tokio::net::UdpSocket,
    /// The ICE-TCP send half, if any.
    pub tcp: Option<&'a dyn TcpSend>,
}

impl std::fmt::Debug for Egress<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Egress")
            .field("socket", &self.socket)
            .field("tcp", &self.tcp.is_some())
            .finish()
    }
}

impl<'a> Egress<'a> {
    /// UDP only (the socket-owning drivers).
    #[must_use]
    pub fn udp(socket: &'a tokio::net::UdpSocket) -> Self {
        Self { socket, tcp: None }
    }
}

fn egress_of<'a, T: SharedTcp>(
    socket: &'a tokio::net::UdpSocket,
    tcp: Option<&'a T>,
) -> Egress<'a> {
    Egress {
        socket,
        tcp: match tcp {
            Some(t) => Some(t),
            None => None,
        },
    }
}

/// The absent ICE-TCP lane of [`UnifiedEndpoint::run_shared`].
enum NoTcp {}

impl TcpSend for NoTcp {
    fn send(&self, _peer: SocketAddr, _payload: &[u8]) -> bool {
        match *self {}
    }
}

impl SharedTcp for NoTcp {
    fn poll_next(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<InboundFrame>> {
        match *self {}
    }
    fn try_next(&mut self) -> Option<InboundFrame> {
        match *self {}
    }
}

/// The single-socket WebRTC endpoint: ONE bound dual-stack UDP socket and ONE
/// TURN relay driver, shared by every WebRTC role (ADR-0048 §4).
///
/// Build with [`UnifiedEndpoint::bind`], register the roles your config needs
/// (preview egress, WHIP ingest, WHEP-serve outputs, `whip_push` outputs) via the
/// returned [`UnifiedBuilder`], then run the one driver task with
/// [`UnifiedEndpoint::run`].
pub struct UnifiedEndpoint {
    endpoint: WebRtcEndpoint,
    /// The single TURN relay driver shared by every role (one allocation set).
    config: EndpointConfig,
    /// The concrete host candidates str0m gathered (IPv6-first), shared by every
    /// role. Inbound datagrams' local destination is resolved onto one of these so
    /// str0m's STUN matching accepts them (box-validation defect #3).
    host_candidates: Vec<SocketAddr>,
    /// The passive ICE-TCP candidates every role advertises (empty unless
    /// `ice_tcp_port` is set on a front-door endpoint). An inbound frame's local
    /// address is resolved onto one of these.
    tcp_candidates: Vec<SocketAddr>,
    /// The WHIP ingest lane (registered when the config has `webrtc` sources).
    ingest: Option<IngestLane>,
    /// The WHEP output-serve lane (registered when the config has `webrtc` outputs).
    serve: Option<ServeLane>,
    /// The native WHEP preview egress (registered when preview is wired).
    preview: Option<Arc<crate::whep_egress::WhepEgress>>,
    /// The `whip_push` client lanes (one per configured `whip_push` output).
    push: Vec<PushLane>,
}

impl std::fmt::Debug for UnifiedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedEndpoint")
            .field("endpoint", &self.endpoint)
            .field("has_ingest", &self.ingest.is_some())
            .field("has_serve", &self.serve.is_some())
            .field("has_preview", &self.preview.is_some())
            .field("push_lanes", &self.push.len())
            .finish_non_exhaustive()
    }
}

/// A builder that wires the WebRTC roles onto the one shared socket before the
/// driver task is spawned. Each `with_*` call returns the role's negotiation
/// handle (or the shared preview egress) so the cli can connect it to the control
/// plane / pipeline, exactly as the per-role endpoints did — but all on one socket.
pub struct UnifiedBuilder {
    endpoint: UnifiedEndpoint,
    host_candidates: Vec<SocketAddr>,
    tcp_candidates: Vec<SocketAddr>,
}

impl std::fmt::Debug for UnifiedBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedBuilder")
            .field("host_candidates", &self.host_candidates)
            .finish_non_exhaustive()
    }
}

impl UnifiedEndpoint {
    /// Bind the single dual-stack socket and start a [`UnifiedBuilder`].
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(mut config: EndpointConfig) -> Result<UnifiedBuilder> {
        // The socket-owning driver ([`run`](Self::run)) has no ICE-TCP lane:
        // never advertise a TCP candidate nothing serves.
        config.ice_tcp_port = None;
        Self::builder(config.clone(), WebRtcEndpoint::bind(config)?)
    }

    /// Start a [`UnifiedBuilder`] on a socket shared with a front door
    /// (ADR-0107): `socket` is a send descriptor of the shared socket, and the
    /// endpoint is then driven with [`run_shared`](Self::run_shared).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Config`] / [`WebRtcError::Socket`] as for
    /// [`WebRtcEndpoint::from_socket`].
    pub fn with_shared_socket(
        config: EndpointConfig,
        socket: std::net::UdpSocket,
    ) -> Result<UnifiedBuilder> {
        Self::builder(config.clone(), WebRtcEndpoint::from_socket(config, socket)?)
    }

    fn builder(config: EndpointConfig, endpoint: WebRtcEndpoint) -> Result<UnifiedBuilder> {
        let host_candidates = endpoint.host_candidates()?;
        let tcp_candidates =
            crate::transport::tcp_passive_candidates(&host_candidates, config.ice_tcp_port);
        Ok(UnifiedBuilder {
            tcp_candidates: tcp_candidates.clone(),
            endpoint: Self {
                endpoint,
                config,
                host_candidates: host_candidates.clone(),
                tcp_candidates,
                ingest: None,
                serve: None,
                preview: None,
                push: Vec::new(),
            },
            host_candidates,
        })
    }

    /// The gathered host candidate addresses (IPv6-first), shared by every role.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn host_candidates(&self) -> Result<Vec<SocketAddr>> {
        self.endpoint.host_candidates()
    }
}

impl UnifiedBuilder {
    /// The gathered host candidate addresses (IPv6-first), shared by every role.
    #[must_use]
    pub fn host_candidates(&self) -> &[SocketAddr] {
        &self.host_candidates
    }

    /// The passive ICE-TCP candidates every role advertises (empty without
    /// `ice_tcp_port`).
    #[must_use]
    pub fn tcp_candidates(&self) -> &[SocketAddr] {
        &self.tcp_candidates
    }

    /// The local address the single shared socket is bound to (the relay
    /// candidate's `raddr` base; the bound host candidate the preview egress uses).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.endpoint.local_addr()
    }

    /// Register the WHIP **ingest** lane on the shared socket, returning the
    /// [`WhipHandle`] the control plane negotiates publishers against.
    #[must_use]
    pub fn with_ingest(mut self) -> (Self, WhipHandle) {
        let (handle, commands, shared) = WhipHandle::build(
            &self.endpoint.config,
            self.host_candidates.clone(),
            self.tcp_candidates.clone(),
        );
        self.endpoint.ingest = Some(IngestLane {
            commands,
            shared,
            sessions: Vec::new(),
        });
        (self, handle)
    }

    /// Register the WHEP **output-serve** lane on the shared socket, returning the
    /// [`WhepServeHandle`] the control plane negotiates viewers against.
    #[must_use]
    pub fn with_serve(mut self) -> (Self, WhepServeHandle) {
        let (handle, commands, shared) = WhepServeHandle::build(
            &self.endpoint.config,
            self.host_candidates.clone(),
            self.tcp_candidates.clone(),
        );
        self.endpoint.serve = Some(ServeLane {
            commands,
            shared,
            viewers: Vec::new(),
        });
        (self, handle)
    }

    /// Register the native **WHEP preview egress** on the shared socket. The
    /// returned [`WhepEgress`](crate::whep_egress::WhepEgress) is the transport the
    /// cli's `CliWhepProvider` negotiates preview viewers against; the driver pumps
    /// its egress + fans inbound datagrams to it.
    #[must_use]
    pub fn with_preview(mut self, preview: Arc<crate::whep_egress::WhepEgress>) -> Self {
        self.endpoint.preview = Some(preview);
        self
    }

    /// Register one `whip_push` **output** on the shared socket: the program egress
    /// `feed` it publishes, whether it carries audio, and the HTTP `signaller` that
    /// `POST`s the offer to the remote WHIP origin.
    #[must_use]
    pub fn with_push(
        mut self,
        feed: EgressFeed,
        audio: bool,
        signaller: Box<dyn crate::transport::whip_push::WhipSignaller>,
    ) -> Self {
        self.endpoint.push.push(PushLane::new(
            WhipPushSpec {
                feed,
                audio,
                host_candidates: self.host_candidates.clone(),
                tcp_candidates: self.tcp_candidates.clone(),
            },
            signaller,
        ));
        self
    }

    /// Finish wiring and return the [`UnifiedEndpoint`] ready to [`run`](UnifiedEndpoint::run).
    #[must_use]
    pub fn build(self) -> UnifiedEndpoint {
        self.endpoint
    }
}

impl UnifiedEndpoint {
    /// Run the single driver loop until `stop` is raised: ONE socket, ONE TURN
    /// driver, every role's sessions demuxed by str0m on each inbound datagram and
    /// pumped on each tick (ADR-0048 §4/§7). The live socket loop is hardware-gated
    /// for full media flow; the structure (demux, relay framing, isolation) is
    /// proven offline.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    pub async fn run(self, stop: Arc<AtomicBool>) -> Result<()> {
        self.run_inner(stop, &RunProbe::default()).await
    }

    /// The driver loop, threading a [`RunProbe`] so a soak test can assert the
    /// invariant-#10 ordering guarantee (servicing keeps pace with receive
    /// budgets). [`run`](Self::run) calls this with a throwaway probe.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    async fn run_inner(self, stop: Arc<AtomicBool>, probe: &RunProbe) -> Result<()> {
        self.run_inner_body(stop, probe, MAX_DRAIN_TIME, DRIVER_TICK)
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one select! loop owning the single socket per ADR-0048 §4 — \
                  splitting the arms would re-introduce per-role drivers"
    )]
    async fn run_inner_body(
        self,
        stop: Arc<AtomicBool>,
        probe: &RunProbe,
        time_budget: Duration,
        // The idle-tick period. Production always passes `DRIVER_TICK`; the
        // flood test passes a period far longer than the test so that, after
        // the interval's immediate first tick, the exhaustion handoff is the
        // ONLY path that drains commands, fires timers, pumps and reaps — so
        // an effect it observes is attributable to the handoff (PR193-02-R2).
        tick_period: Duration,
    ) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        // The concrete candidates an inbound datagram's local destination may be
        // resolved onto — the gathered host candidates with the unspecified `[::]`
        // bind addr removed (str0m never gathers it and discards STUN to it).
        let concrete_candidates: Vec<SocketAddr> = self
            .host_candidates
            .iter()
            .copied()
            .filter(|a| !a.ip().is_unspecified())
            .collect();
        let mut turn = TurnRelayDriver::from_config(&self.config, Instant::now());

        let std_socket = self.endpoint.into_socket();
        std_socket
            .set_nonblocking(true)
            .map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let socket =
            tokio::net::UdpSocket::from_std(std_socket).map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        // Ask the kernel to report each datagram's concrete destination IP via
        // `IPV6_PKTINFO` / `IP_PKTINFO` so the unspecified-bound socket can still
        // tell str0m the local candidate the packet arrived on (defect #3). A
        // failure here is fatal: without PKTINFO every inbound STUN would be
        // discarded as "unknown interface" and ICE could never complete.
        crate::transport::local_addr::enable_pktinfo(&socket2::SockRef::from(&socket)).map_err(
            |source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            },
        )?;

        let mut ingest = self.ingest;
        let mut serve = self.serve;
        let preview = self.preview;
        let mut push = self.push;

        let mut buf = vec![0u8; RECV_BUFFER];
        let mut tick = tokio::time::interval(tick_period);
        // A late tick runs once and the cadence restarts from it (no burst).
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The socket-owning driver has no ICE-TCP lane (UDP only).
        let egress = Egress::udp(&socket);

        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }

            // `biased`: poll the tick arm before the receive arm so a permanently
            // ready socket can never crowd the idle tick out of the schedule. The
            // receive arm additionally performs an explicit bounded handoff after
            // an exhausted budget (below), so servicing does not depend on this
            // bias alone — but biasing keeps the steady-state tick cadence tight
            // even mid-flood (invariant #10).
            tokio::select! {
                biased;
                _ = tick.tick() => {
                    let now = Instant::now();
                    Self::service_timers_and_gc(
                        egress, &mut turn,
                        &mut ingest, &mut serve, preview.as_ref(), &mut push,
                        local_addr, now, probe,
                        #[cfg(test)]
                        ServiceOrigin::IdleTick,
                    ).await;
                }
                ready = socket.readable() => {
                    let now = Instant::now();
                    let outcome = if ready.is_ok() {
                        // Drain at most one wake's BUDGET of datagrams (count +
                        // time), then break to hand off — never an unbounded drain
                        // a sustained/hostile flood could ride forever and starve
                        // the rest of the loop (invariant #10, MAJOR #1). Each read
                        // recovers the CONCRETE local destination via PKTINFO and
                        // resolves it onto a gathered candidate so str0m's STUN
                        // matching accepts the check (defect #3).
                        Self::drain_inbound_budgeted_with(
                            &socket, &mut buf, local_addr, &concrete_candidates,
                            time_budget,
                            |src, dst, payload| {
                                Self::route_inbound(
                                    &mut turn,
                                    ingest.as_mut(),
                                    serve.as_mut(),
                                    preview.as_ref(),
                                    &mut push,
                                    src,
                                    dst,
                                    payload,
                                    now,
                                );
                            },
                        )
                    } else {
                        DrainOutcome { read_count: 0, budget_exhausted: false }
                    };

                    if outcome.budget_exhausted {
                        // The budget cut the drain short while the socket still
                        // holds data, so the `readable()` arm would re-fire at
                        // once. Perform an explicit BOUNDED HANDOFF before that can
                        // happen: yield to the runtime, then run ONE full servicing
                        // pump (commands + timers + outbound + GC). This makes the
                        // rest of the driver run GUARANTEED between consecutive
                        // receive budgets — it does not rely on `select!` happening
                        // to pick the tick arm (invariant #10, round-2 BLOCKER).
                        probe
                            .recv_budgets_exhausted
                            .fetch_add(1, Ordering::Relaxed);
                        #[cfg(test)]
                        {
                            let run = probe.dry_free_run.fetch_add(1, Ordering::Relaxed) + 1;
                            probe.max_dry_free_run.fetch_max(run, Ordering::Relaxed);
                        }
                        tokio::task::yield_now().await;
                        Self::service_timers_and_gc(
                            egress, &mut turn,
                            &mut ingest, &mut serve, preview.as_ref(), &mut push,
                            local_addr, now, probe,
                            #[cfg(test)]
                            ServiceOrigin::ExhaustionHandoff,
                        ).await;
                    } else {
                        #[cfg(test)]
                        {
                            probe.drained_dry.fetch_add(1, Ordering::Relaxed);
                            probe.dry_free_run.store(0, Ordering::Relaxed);
                        }
                        // The socket drained within budget: just pump outbound
                        // (the full tick servicing runs on the idle tick arm). No
                        // re-arm is needed — tokio cleared readiness.
                        Self::pump_all(
                            egress, &mut turn,
                            ingest.as_mut(), serve.as_mut(), preview.as_ref(), &mut push,
                            local_addr, now,
                            #[cfg(test)]
                            probe,
                            #[cfg(test)]
                            ServiceOrigin::DrainedDry,
                        ).await;
                    }
                }
            }
        }
    }

    /// Run the driver on a socket shared through a front door (ADR-0107 slice
    /// 1): receive from `inbound` (never from the socket), send on the
    /// endpoint's own descriptor (a duplicate of the shared socket), and report
    /// validated peers back so the dispatcher can tell established flows from
    /// new ones. Same bounded per-wake budget and servicing handoff as
    /// [`run`](Self::run). Spawn it on the front door's runtime. No ICE-TCP:
    /// see [`run_shared_with_tcp`](Self::run_shared_with_tcp).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the send socket to async fails.
    pub async fn run_shared<I: SharedInbound>(
        self,
        inbound: I,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        self.run_shared_with_tcp(inbound, None::<NoTcp>, stop).await
    }

    /// [`run_shared`](Self::run_shared) plus the front door's ICE-TCP lane
    /// (slice 4): deframed frames from `tcp` reach every role as
    /// [`Transport::Tcp`] on the gathered passive TCP candidate, and every
    /// [`Transport::Tcp`] transmit goes to [`TcpSend::send`] (synchronous,
    /// never blocks on a peer). A lane that ends (`Ready(None)`) is no longer
    /// polled; UDP keeps running.
    ///
    /// Scheduling (invariant #10): the `select!` is biased tick → UDP → TCP,
    /// and the tick uses [`MissedTickBehavior::Delay`](tokio::time::MissedTickBehavior::Delay)
    /// (a late tick never bursts). Each lane wake drains at most one budget
    /// ([`MAX_DATAGRAMS_PER_WAKE`] items / [`MAX_DRAIN_TIME`]). When a wake
    /// exhausts its budget, the OTHER lane is drained once, without waiting,
    /// with the same budget (`try_next` loop) — so a permanently ready lane
    /// can never starve the other: every lane is served at least once per two
    /// budgets. An exhausted wake (either lane) is then followed by
    /// `yield_now` and one full servicing pass (the bounded handoff), and the
    /// biased tick arm is polled first on the next iteration.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the send socket to async fails.
    pub async fn run_shared_with_tcp<I: SharedInbound, T: SharedTcp>(
        self,
        inbound: I,
        tcp: Option<T>,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        self.run_shared_body(inbound, tcp, stop, &RunProbe::default())
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one select! loop, mirroring run_inner_body for the shared-socket input"
    )]
    async fn run_shared_body<I: SharedInbound, T: SharedTcp>(
        self,
        mut inbound: I,
        mut tcp: Option<T>,
        stop: Arc<AtomicBool>,
        probe: &RunProbe,
    ) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        let concrete_candidates: Vec<SocketAddr> = self
            .host_candidates
            .iter()
            .copied()
            .filter(|a| !a.ip().is_unspecified())
            .collect();
        let tcp_candidates = self.tcp_candidates;
        let mut turn = TurnRelayDriver::from_config(&self.config, Instant::now());
        let std_socket = self.endpoint.into_socket();
        std_socket
            .set_nonblocking(true)
            .map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let socket =
            tokio::net::UdpSocket::from_std(std_socket).map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let mut ingest = self.ingest;
        let mut serve = self.serve;
        let preview = self.preview;
        let mut push = self.push;
        let mut tick = tokio::time::interval(DRIVER_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Bounded by the session caps: one entry per validated session peer.
        let mut reported: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
        let mut current: Vec<SocketAddr> = Vec::new();
        // The TCP lane ended (the door is gone): stop polling it.
        let mut tcp_done = tcp.is_none();

        loop {
            if stop.load(Ordering::Acquire) {
                for peer in reported.drain() {
                    inbound.invalidated(peer);
                }
                return Ok(());
            }
            let exhausted = tokio::select! {
                biased;
                _ = tick.tick() => {
                    let now = Instant::now();
                    let egress = egress_of(&socket, tcp.as_ref());
                    Self::service_timers_and_gc(
                        egress, &mut turn,
                        &mut ingest, &mut serve, preview.as_ref(), &mut push,
                        local_addr, now, probe,
                        #[cfg(test)]
                        ServiceOrigin::IdleTick,
                    ).await;
                    current.clear();
                    Self::validated_peers(ingest.as_ref(), serve.as_ref(), preview.as_ref(), &push, &mut current);
                    Self::sync_validated(&mut inbound, &mut reported, &current);
                    continue;
                }
                first = std::future::poll_fn(|cx| inbound.poll_next(cx)) => {
                    let Some(first) = first else {
                        // The dispatcher is gone: the front door shut down.
                        return Ok(());
                    };
                    let now = Instant::now();
                    let mut exhausted = Self::drain_udp_lane(
                        &mut inbound, Some(first), &mut turn,
                        &mut ingest, &mut serve, preview.as_ref(), &mut push,
                        local_addr, &concrete_candidates, now,
                    );
                    if exhausted {
                        // Fairness: a saturated UDP lane must not starve TCP.
                        if let Some(lane) = tcp.as_mut().filter(|_| !tcp_done) {
                            exhausted |= Self::drain_tcp_lane(
                                lane, None,
                                &mut ingest, &mut serve, preview.as_ref(), &mut push,
                                &tcp_candidates, now,
                            );
                        }
                    }
                    exhausted
                }
                frame = std::future::poll_fn(|cx| match tcp.as_mut() {
                    Some(lane) if !tcp_done => lane.poll_next(cx),
                    _ => std::task::Poll::Pending,
                }) => {
                    let Some(first) = frame else {
                        // The TCP door is gone; UDP keeps running.
                        tcp_done = true;
                        continue;
                    };
                    let now = Instant::now();
                    let Some(lane) = tcp.as_mut() else { continue };
                    let mut exhausted = Self::drain_tcp_lane(
                        lane, Some(first),
                        &mut ingest, &mut serve, preview.as_ref(), &mut push,
                        &tcp_candidates, now,
                    );
                    if exhausted {
                        // Fairness: a saturated TCP lane must not starve UDP.
                        exhausted |= Self::drain_udp_lane(
                            &mut inbound, None, &mut turn,
                            &mut ingest, &mut serve, preview.as_ref(), &mut push,
                            local_addr, &concrete_candidates, now,
                        );
                    }
                    exhausted
                }
            };
            let now = Instant::now();
            let egress = egress_of(&socket, tcp.as_ref());
            if exhausted {
                // Budget spent with input possibly still queued: the same
                // bounded handoff as the socket-owning driver.
                probe.recv_budgets_exhausted.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                Self::service_timers_and_gc(
                    egress,
                    &mut turn,
                    &mut ingest,
                    &mut serve,
                    preview.as_ref(),
                    &mut push,
                    local_addr,
                    now,
                    probe,
                    #[cfg(test)]
                    ServiceOrigin::ExhaustionHandoff,
                )
                .await;
            } else {
                Self::pump_all(
                    egress,
                    &mut turn,
                    ingest.as_mut(),
                    serve.as_mut(),
                    preview.as_ref(),
                    &mut push,
                    local_addr,
                    now,
                    #[cfg(test)]
                    probe,
                    #[cfg(test)]
                    ServiceOrigin::DrainedDry,
                )
                .await;
            }
        }
    }

    /// Drain at most one budget of shared-socket datagrams, starting with
    /// `first` (if any) and continuing with `try_next` (never waits). Returns
    /// whether the budget (count or time) was exhausted.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single demux fans one datagram to every role lane"
    )]
    fn drain_udp_lane<I: SharedInbound>(
        inbound: &mut I,
        first: Option<InboundDatagram>,
        turn: &mut TurnRelayDriver,
        ingest: &mut Option<IngestLane>,
        serve: &mut Option<ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        local_addr: SocketAddr,
        concrete_candidates: &[SocketAddr],
        now: Instant,
    ) -> bool {
        let started = Instant::now();
        let mut next = first.or_else(|| inbound.try_next());
        let mut read = 0usize;
        while let Some(d) = next.take() {
            read += 1;
            // No PKTINFO on a wildcard-bound socket: the concrete local
            // destination is unknown, so drop the datagram rather than map it
            // onto a guessed candidate, exactly as the socket-owning driver
            // does (defect #3).
            if let Some(dst_ip) = d
                .dst_ip
                .or_else(|| (!local_addr.ip().is_unspecified()).then(|| local_addr.ip()))
            {
                let arrival = SocketAddr::new(dst_ip, local_addr.port());
                let dst = crate::transport::resolve_local_destination(arrival, concrete_candidates);
                Self::route_inbound(
                    turn,
                    ingest.as_mut(),
                    serve.as_mut(),
                    preview,
                    push,
                    d.src,
                    dst,
                    &d.payload,
                    now,
                );
            }
            if read < MAX_DATAGRAMS_PER_WAKE && started.elapsed() < MAX_DRAIN_TIME {
                next = inbound.try_next();
            }
        }
        read >= MAX_DATAGRAMS_PER_WAKE || started.elapsed() >= MAX_DRAIN_TIME
    }

    /// Drain at most one budget of ICE-TCP frames, starting with `first` (if
    /// any) and continuing with `try_next` (never waits). Returns whether the
    /// budget (count or time) was exhausted.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single demux fans one frame to every role lane"
    )]
    fn drain_tcp_lane<T: SharedTcp>(
        lane: &mut T,
        first: Option<InboundFrame>,
        ingest: &mut Option<IngestLane>,
        serve: &mut Option<ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        tcp_candidates: &[SocketAddr],
        now: Instant,
    ) -> bool {
        let started = Instant::now();
        let mut next = first.or_else(|| lane.try_next());
        let mut read = 0usize;
        while let Some(f) = next.take() {
            read += 1;
            // No TURN classification: TURN is UDP here. The frame's local
            // address is resolved onto the advertised passive TCP candidate
            // (NAT 1:1 / Docker as for UDP); with no TCP candidate advertised
            // nothing can accept it.
            if !tcp_candidates.is_empty() {
                let dst = crate::transport::resolve_local_destination(f.local, tcp_candidates);
                Self::route_to_roles(
                    ingest.as_mut(),
                    serve.as_mut(),
                    preview,
                    push,
                    Transport::Tcp,
                    f.peer,
                    dst,
                    &f.payload,
                    now,
                );
            }
            if read < MAX_DATAGRAMS_PER_WAKE && started.elapsed() < MAX_DRAIN_TIME {
                next = lane.try_next();
            }
        }
        read >= MAX_DATAGRAMS_PER_WAKE || started.elapsed() >= MAX_DRAIN_TIME
    }

    /// Every connected session's ICE-validated remote address, across roles.
    fn validated_peers(
        ingest: Option<&IngestLane>,
        serve: Option<&ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &[PushLane],
        out: &mut Vec<SocketAddr>,
    ) {
        if let Some(lane) = ingest {
            WhipEndpoint::validated_peers(&lane.sessions, out);
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::validated_peers(&lane.viewers, out);
        }
        if let Some(preview) = preview {
            out.extend(preview.validated_peers());
        }
        out.extend(push.iter().filter_map(PushLane::validated_peer));
        // One key per peer however the dual-stack socket reported it (an IPv4
        // peer arrives IPv4-mapped on the socket path, native IPv4 through a
        // TURN relay), and one entry per address however many sessions use it.
        for peer in out.iter_mut() {
            *peer = canonical_peer(*peer);
        }
        out.sort_unstable();
        out.dedup();
    }

    /// Report the difference between `current` and what was last reported.
    fn sync_validated<I: SharedInbound>(
        inbound: &mut I,
        reported: &mut std::collections::HashSet<SocketAddr>,
        current: &[SocketAddr],
    ) {
        // `current` is canonical, sorted and deduplicated.
        reported.retain(|peer| {
            let keep = current.binary_search(peer).is_ok();
            if !keep {
                inbound.invalidated(*peer);
            }
            keep
        });
        for peer in current {
            if !reported.contains(peer) && inbound.validated(*peer) {
                reported.insert(*peer);
            }
        }
    }

    /// One full servicing pass: drain each lane's register/release commands,
    /// advance every role's ICE/DTLS timers + feeds, pump outbound onto the one
    /// socket, then GC dead sessions. Run once per idle tick **and** once per
    /// exhausted receive budget (the invariant-#10 bounded handoff), so a
    /// permanent inbound flood can never defer command/timer/GC servicing past
    /// one receive budget.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single servicing pass drives every role on one socket"
    )]
    async fn service_timers_and_gc(
        egress: Egress<'_>,
        turn: &mut TurnRelayDriver,
        ingest: &mut Option<IngestLane>,
        serve: &mut Option<ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        local_addr: SocketAddr,
        now: Instant,
        probe: &RunProbe,
        #[cfg(test)] origin: ServiceOrigin,
    ) {
        probe.driver_services.fetch_add(1, Ordering::Relaxed);
        // Drain pending register/release commands for each lane.
        Self::drain_commands(ingest.as_mut(), serve.as_mut());
        // Advance each role's timers / feeds.
        if let Some(lane) = ingest.as_mut() {
            WhipEndpoint::tick(&mut lane.sessions, now);
        }
        if let Some(lane) = serve.as_mut() {
            WhepServeEndpoint::tick(&mut lane.viewers, &lane.shared, now);
        }
        Self::pump_all(
            egress,
            turn,
            ingest.as_mut(),
            serve.as_mut(),
            preview,
            push,
            local_addr,
            now,
            #[cfg(test)]
            probe,
            #[cfg(test)]
            origin,
        )
        .await;
        // GC dead sessions per lane.
        if let Some(lane) = ingest.as_mut() {
            WhipEndpoint::reap(&mut lane.sessions, &lane.shared);
        }
        if let Some(lane) = serve.as_mut() {
            WhepServeEndpoint::reap(&mut lane.viewers, &lane.shared);
        }
        // `cfg(test)`-only (review PR193-04): counts completed exhaustion
        // handoffs, gated on the origin the caller threads through, so a
        // missing handoff call or an early return before this point desyncs it
        // from `recv_budgets_exhausted`. It does NOT prove the work above ran;
        // the flood test asserts each step's own effect for that (PR193-02-R2).
        #[cfg(test)]
        if origin == ServiceOrigin::ExhaustionHandoff {
            probe.handoff_services.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(test)]
        if origin == ServiceOrigin::IdleTick {
            probe.idle_tick_services.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain (non-blocking) every pending register/release command for the lanes,
    /// so a registration is admitted promptly without an awaited per-lane arm.
    fn drain_commands(ingest: Option<&mut IngestLane>, serve: Option<&mut ServeLane>) {
        if let Some(lane) = ingest {
            while let Ok(cmd) = lane.commands.try_recv() {
                WhipEndpoint::apply_command(&mut lane.sessions, Some(cmd));
            }
        }
        if let Some(lane) = serve {
            while let Ok(cmd) = lane.commands.try_recv() {
                WhepServeEndpoint::apply_command(&mut lane.viewers, Some(cmd));
            }
        }
    }

    /// Drain the readable socket for **at most one wake's budget** of datagrams,
    /// invoking `route` for each, then return how the drain ended (invariant #10,
    /// MAJOR #1).
    ///
    /// The drain stops at the **first** of: the socket draining (`WouldBlock`),
    /// the [`MAX_DATAGRAMS_PER_WAKE`] count cap, or the [`MAX_DRAIN_TIME`] time
    /// budget. A per-datagram receive error other than `WouldBlock` (a malformed
    /// datagram, or `MSG_CTRUNC` from [`recv_from_with_local`]) **drops that one
    /// datagram and continues** — it still counts against the budget, so a hostile
    /// peer spraying malformed datagrams cannot get unbounded reads, and one bad
    /// datagram never halts the drain of good ones.
    ///
    /// [`DrainOutcome::budget_exhausted`] tells the caller the socket may still
    /// have data. On exhaustion the driver performs an explicit bounded handoff —
    /// [`yield_now`](tokio::task::yield_now) then exactly one
    /// [`service_timers_and_gc`](Self::service_timers_and_gc) pass — **before**
    /// the `readable` arm can re-fire, so commands/timers/outbound/GC are
    /// serviced once per exhausted budget regardless of `select!` arm ordering,
    /// and the loop's top-of-iteration stop check is reached within a bounded
    /// number of iterations. The flood gets bounded work per wake and cannot
    /// defer the rest of the driver past one receive budget.
    ///
    /// [`recv_from_with_local`]: crate::transport::local_addr::recv_from_with_local
    #[cfg(test)]
    fn drain_inbound_budgeted(
        socket: &tokio::net::UdpSocket,
        buf: &mut [u8],
        local_addr: SocketAddr,
        concrete_candidates: &[SocketAddr],
        route: impl FnMut(SocketAddr, SocketAddr, &[u8]),
    ) -> DrainOutcome {
        Self::drain_inbound_budgeted_with(
            socket,
            buf,
            local_addr,
            concrete_candidates,
            MAX_DRAIN_TIME,
            route,
        )
    }

    fn drain_inbound_budgeted_with(
        socket: &tokio::net::UdpSocket,
        buf: &mut [u8],
        local_addr: SocketAddr,
        concrete_candidates: &[SocketAddr],
        time_budget: Duration,
        mut route: impl FnMut(SocketAddr, SocketAddr, &[u8]),
    ) -> DrainOutcome {
        let started = Instant::now();
        let mut read_count = 0usize;
        loop {
            if read_count >= MAX_DATAGRAMS_PER_WAKE || started.elapsed() >= time_budget {
                // Budget hit while the socket may still hold data: break to
                // pump/yield; the caller re-arms the `select!` (defect #1 fix).
                return DrainOutcome {
                    read_count,
                    budget_exhausted: true,
                };
            }
            let read = socket.try_io(tokio::io::Interest::READABLE, || {
                crate::transport::local_addr::recv_from_with_local(
                    &socket2::SockRef::from(socket),
                    buf,
                    local_addr,
                )
            });
            match read {
                Ok((len, src, arrival)) => {
                    read_count += 1;
                    let dst =
                        crate::transport::resolve_local_destination(arrival, concrete_candidates);
                    if let Some(payload) = buf.get(..len) {
                        route(src, dst, payload);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Socket drained: tokio cleared readiness, so the next
                    // `select!` `readable()` genuinely waits. No re-arm needed.
                    return DrainOutcome {
                        read_count,
                        budget_exhausted: false,
                    };
                }
                Err(_) => {
                    // A malformed/truncated datagram (e.g. MSG_CTRUNC): drop just
                    // this one, count it against the budget, keep draining the
                    // rest. A hostile peer cannot halt the drain or escape the cap.
                    read_count += 1;
                }
            }
        }
    }

    /// Route one inbound datagram to the role whose session accepts it. The relay
    /// classification (TURN control / relayed Data / media) happens once; relayed
    /// media is fanned to every role exactly like direct media (str0m's per-session
    /// ufrag/peer demux silently ignores a datagram not addressed to a session).
    #[allow(
        clippy::too_many_arguments,
        reason = "the single demux fans one datagram to every role lane"
    )]
    fn route_inbound(
        turn: &mut TurnRelayDriver,
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        src: SocketAddr,
        local_dst: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        // Classify once: a relayed Data indication decapsulates to media arriving
        // on the RELAY candidate addr (so str0m's relay candidate pair validates);
        // a TURN control reply feeds the relay driver; anything else is media from
        // `src` arriving on `local_dst` — the CONCRETE local candidate the caller
        // resolved from the datagram's PKTINFO destination, never the unspecified
        // `[::]` bind (defect #3 / defect C).
        let (route_src, route_dst, bytes): (SocketAddr, SocketAddr, Vec<u8>) =
            match crate::transport::relay_io::classify_inbound(turn, src, payload, now) {
                crate::transport::relay_io::Inbound::Relayed {
                    peer,
                    relay,
                    payload,
                } => (peer, relay, payload),
                crate::transport::relay_io::Inbound::TurnControl => return,
                crate::transport::relay_io::Inbound::Media => (src, local_dst, payload.to_vec()),
            };

        Self::route_to_roles(
            ingest,
            serve,
            preview,
            push,
            Transport::Udp,
            route_src,
            route_dst,
            &bytes,
            now,
        );
    }

    /// Fan one datagram (UDP, relay-decapsulated, or an ICE-TCP frame) to every
    /// registered role; str0m's demux means only the owning session consumes it.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single demux fans one datagram to every role lane"
    )]
    fn route_to_roles(
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        proto: Transport,
        src: SocketAddr,
        dst: SocketAddr,
        bytes: &[u8],
        now: Instant,
    ) {
        if let Some(lane) = ingest {
            WhipEndpoint::route_datagram(&mut lane.sessions, proto, src, dst, bytes, now);
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::route_datagram(&mut lane.viewers, proto, src, dst, bytes, now);
        }
        if let Some(preview) = preview {
            let _ = preview.handle_datagram_broadcast_proto(proto, src, dst, bytes, now);
        }
        for lane in push.iter_mut() {
            lane.handle_inbound_proto(proto, src, dst, bytes, now);
        }
    }

    /// Pump every role's outbound + the shared TURN driver onto the one socket.
    #[allow(
        clippy::too_many_arguments,
        reason = "the single loop drives every role on one socket"
    )]
    async fn pump_all(
        egress: Egress<'_>,
        turn: &mut TurnRelayDriver,
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        local_addr: SocketAddr,
        now: Instant,
        #[cfg(test)] probe: &RunProbe,
        #[cfg(test)] origin: ServiceOrigin,
    ) {
        // Drive the shared TURN driver first so a fresh relay is published before
        // the roles gather candidates for the next negotiation.
        while let Some((destination, payload)) = turn.poll_transmit(now) {
            let _ = egress.socket.send_to(&payload, destination).await;
        }
        let new_relays = turn.take_new_relays();
        if !new_relays.is_empty() {
            Self::publish_relays(
                &new_relays,
                ingest.as_deref(),
                serve.as_deref(),
                preview,
                local_addr,
            );
        }

        if let Some(lane) = ingest {
            WhipEndpoint::pump_outbound(
                egress,
                &mut lane.sessions,
                turn,
                now,
                #[cfg(test)]
                &mut |dst, payload| probe.record_send(origin, dst, payload),
            )
            .await;
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::pump_outbound(egress, &mut lane.viewers, turn, now).await;
        }
        if let Some(preview) = preview {
            if let Ok(out) = preview.drive_all_routed(now) {
                for dg in out {
                    crate::transport::relay_io::send_routed(egress, turn, &dg, now).await;
                }
            }
        }
        for lane in push.iter_mut() {
            lane.step(egress, turn, local_addr, now).await;
        }
    }

    /// Publish freshly-learned relays to each role's relay-candidate sink so the
    /// next negotiation offers them (WHIP/WHEP-serve via `learned_relays`, preview
    /// via `WhepEgress::learn_relay`).
    fn publish_relays(
        relays: &[SocketAddr],
        ingest: Option<&IngestLane>,
        serve: Option<&ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        local_addr: SocketAddr,
    ) {
        if let Some(lane) = ingest {
            lane.shared.push_relays(relays);
        }
        if let Some(lane) = serve {
            lane.shared.push_relays(relays);
        }
        if let Some(preview) = preview {
            for relay in relays {
                preview.learn_relay(*relay, local_addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording `SharedInbound` (no datagrams), for the validated-peer sync.
    #[derive(Default)]
    struct Recorder {
        events: Vec<(SocketAddr, bool)>,
    }

    impl SharedInbound for Recorder {
        fn poll_next(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<InboundDatagram>> {
            std::task::Poll::Pending
        }
        fn try_next(&mut self) -> Option<InboundDatagram> {
            None
        }
        fn validated(&mut self, peer: SocketAddr) -> bool {
            self.events.push((peer, true));
            true
        }
        fn invalidated(&mut self, peer: SocketAddr) {
            self.events.push((peer, false));
        }
    }

    /// Review finding (PR 315): a peer seen IPv4-mapped on one path and native
    /// IPv4 on another must be one established entry with no per-tick churn.
    #[test]
    fn mapped_and_native_reports_of_one_peer_are_one_entry_without_churn() {
        let mapped: SocketAddr = "[::ffff:192.0.2.9]:5000".parse().unwrap();
        let native: SocketAddr = "192.0.2.9:5000".parse().unwrap();
        let mut rec = Recorder::default();
        let mut reported = std::collections::HashSet::new();
        for _tick in 0..5 {
            let mut current = vec![mapped, native];
            for p in &mut current {
                *p = canonical_peer(*p);
            }
            current.sort_unstable();
            current.dedup();
            UnifiedEndpoint::sync_validated(&mut rec, &mut reported, &current);
        }
        assert_eq!(
            rec.events,
            vec![(native, true)],
            "one insert, no remove/insert churn"
        );
        UnifiedEndpoint::sync_validated(&mut rec, &mut reported, &[]);
        assert_eq!(rec.events.last(), Some(&(native, false)));
    }
    use std::net::{Ipv6Addr, SocketAddrV6, UdpSocket};

    /// Count queued UDP datagrams with Linux's advancing peek cursor. No read
    /// consumes a datagram; restore the default offset before the real drain.
    /// Linux-only inspection path for [`servicing_keeps_pace_with_the_receive_budget_under_flood`]'s
    /// precondition; the guarantee itself (below) is proven on every platform
    /// via a portable OS-thread flooder, not via this inspection (review
    /// PR193-01).
    #[cfg(target_os = "linux")]
    fn queued_datagrams(socket: &tokio::net::UdpSocket) -> std::io::Result<usize> {
        use std::mem::MaybeUninit;

        let socket = socket2::SockRef::from(socket);
        crate::transport::local_addr::set_peek_offset(&socket, 0)?;
        let mut count = 0;
        let mut payload = [MaybeUninit::uninit(); 5];
        while count <= MAX_DATAGRAMS_PER_WAKE {
            match socket.recv_from_with_flags(&mut payload, libc::MSG_PEEK | libc::MSG_DONTWAIT) {
                Ok(_) => count += 1,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        crate::transport::local_addr::set_peek_offset(&socket, -1)?;
        Ok(count)
    }

    /// Stops and joins the flood OS thread on EVERY exit path — normal return,
    /// a failed assertion or a timeout `expect` — so a failing flood test can
    /// never detach a sender that keeps flooding loopback for the rest of the
    /// test process (review PR193-05). Dropping a `JoinHandle` only detaches;
    /// this guard raises both stop flags first, then joins.
    struct FloodGuard {
        stop: Arc<AtomicBool>,
        flood_stop: Arc<AtomicBool>,
        flooders: Vec<std::thread::JoinHandle<()>>,
    }

    impl Drop for FloodGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.flood_stop.store(true, Ordering::Release);
            for flooder in self.flooders.drain(..) {
                // A flooder panic must not double-panic during unwinding; the
                // original failure is what the test reports.
                let _ = flooder.join();
            }
        }
    }

    /// Build a publisher offer with a host candidate (mirrors
    /// `tests/unified_socket.rs::offer`), so the flood test can negotiate a
    /// REAL ingest session and observe a REAL effect of servicing — not just an
    /// entry counter (review PR193-02).
    fn publisher_offer(peer: SocketAddr) -> String {
        let now = Instant::now();
        let mut s =
            crate::transport::Session::new(&crate::transport::SessionConfig::default(), now);
        s.add_host_candidate(peer).expect("host candidate accepted");
        s.create_offer(&[crate::transport::MediaKind::Video])
            .expect("offer built")
    }

    /// Await `cond` while the flood runs, bounded by COUNT: give up once the
    /// driver has exhausted `cap` further receive budgets without `cond`
    /// holding (review PR193-06), with `SAFETY` only as a hang guard. Returns
    /// whether `cond` held.
    async fn within_exhaustions(
        probe: &RunProbe,
        cap: u64,
        safety: Duration,
        mut cond: impl FnMut() -> bool,
    ) -> bool {
        let start = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
        tokio::time::timeout(safety, async {
            loop {
                if cond() {
                    return true;
                }
                if probe.recv_budgets_exhausted.load(Ordering::Relaxed) - start >= cap {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Invariant #10 / round-2 BLOCKER, plus review PR193-02/PR193-03: under a
    /// **sustained, portable** inbound flood the driver's servicing pass
    /// (commands + timers + outbound + GC) must keep pace with the receive
    /// budget, and the runtime itself must stay fair to an unrelated task.
    ///
    /// Checks, each bounded by exhaustion COUNTS (wall clock is only a hang
    /// guard, review PR193-06):
    ///
    /// 1. **Ordering**: `handoff_services == recv_budgets_exhausted` — a
    ///    missing handoff call or an early return before the tail increment
    ///    desyncs the two.
    /// 2. **Servicing effects owned by the handoff** (review PR193-02-R2). The
    ///    driver runs with an idle tick far longer than the test, so after its
    ///    one immediate tick the exhaustion handoff is the only full servicing
    ///    path. Every command is queued only after saturation and a completed
    ///    handoff. Then: a mid-flood Register + the session's ICE timer must
    ///    put a STUN check on the wire to a peer (command drain + timer); a
    ///    check the peer received must be one the handoff's own outbound pump
    ///    sent, attributed at the send site (review PR193-07); a mid-flood
    ///    Release must close the RTP ring (command drain); and the dead session must be closed in the session table,
    ///    which only reap/GC does.
    /// 3. **Independent fairness** (review PR193-03-R2): a separate task on the
    ///    same current-thread runtime records, at each of ITS polls, how many
    ///    exhaustions ran since its previous poll, including the final interval
    ///    up to driver stop. The max must stay `<= MAX_TOLERATED_GAP`. The
    ///    window must contain `REQUIRED_DRY_FREE_RUN` consecutive exhaustions
    ///    with no drain reaching `WouldBlock`, so only the production
    ///    `yield_now` can schedule the observer there (the driver runs
    ///    `unconstrained`): removing it fails the gap assertion.
    ///
    /// Portable (no Linux-only socket inspection in the production guarantee):
    /// a dedicated OS thread (not a tokio worker) floods the bound socket
    /// without pause, keeping `socket.readable()` effectively always ready on
    /// every platform CI runs on (Linux, macOS).
    #[allow(
        clippy::too_many_lines,
        reason = "ordering, handoff servicing effects and runtime fairness share \
                  one flood scenario; splitting them would re-run the flood setup"
    )]
    #[tokio::test(flavor = "current_thread")]
    async fn servicing_keeps_pace_with_the_receive_budget_under_flood() {
        use std::sync::atomic::AtomicBool;

        // How many receive budgets the flood must drive to exhaustion before we
        // stop the driver. Above 1 so the test exercises a *sustained*
        // saturating flood (many consecutive budget->handoff cycles).
        const TARGET_EXHAUSTIONS: u64 = 8;
        // The fairness window is bounded by COUNTS, not time (review
        // PR193-06): it measures at least this many further exhaustions AND
        // must contain one dry-free run of `REQUIRED_DRY_FREE_RUN` exhaustions
        // (see below); `FAIRNESS_EXHAUSTION_CAP` ends a window that never
        // finds one, which then fails the saturation precondition.
        const FAIRNESS_WINDOW_EXHAUSTIONS: u64 = 300;
        const FAIRNESS_EXHAUSTION_CAP: u64 = 5_000;
        // Saturation proof (review PR193-06): within the measured window the
        // socket must stay continuously readable for this many consecutive
        // exhausted budgets — no drain reached `WouldBlock`, so the receive
        // arm never returned `Pending`. Inside such a run the ONLY way the
        // observer can be scheduled is the production `yield_now` (the driver
        // runs `unconstrained`, so Tokio's coop budget cannot do it either);
        // without it the observer's gap is at least this run's length, which
        // is three times `MAX_TOLERATED_GAP`. This turns the no-yield
        // detection from a throughput assumption into a checked precondition.
        // Kept modest because the kernel caps the receive queue (Linux
        // `rmem_max` default ~208 KiB, about one budget of small datagrams);
        // measured on 2 contended cores the longest run per window was 40-41.
        const REQUIRED_DRY_FREE_RUN: u64 = 24;
        // Several sender threads so a descheduled sender cannot let the
        // (default ~200 KiB, about one budget) receive queue run dry on a
        // contended 2-core runner. Bounded and joined by `FloodGuard`.
        const FLOODER_THREADS: usize = 4;
        // The largest tolerated number of exhaustions between two consecutive
        // observer polls, as recorded by the observer itself. With the
        // production `yield_now` on every exhausted budget the current-thread
        // runtime polls the observer between consecutive exhaustions, so the
        // gap is structurally 1-2 (measured 2, idle and on 2 contended cores);
        // without it the gap spans the whole dry-free run (measured 52-84, and
        // at least REQUIRED_DRY_FREE_RUN by construction).
        const MAX_TOLERATED_GAP: u64 = 8;
        // Hang guard ONLY, never the pass/fail bound: every phase ends on a
        // count. The flooder itself stops sending at this deadline (it runs on
        // an OS thread, so this works even if a regression never lets the
        // runtime poll its own timers); the socket then drains, the driver
        // returns `Pending`, and the Tokio timeouts below can fire.
        const SAFETY_TIMEOUT: Duration = Duration::from_secs(120);
        // Count bound for each servicing-effect phase: give up (and fail on
        // the phase's assertion) after this many further exhausted budgets.
        // Correct code needs 0-3 per phase (measured), so this is >300x slack.
        const SERVICE_EXHAUSTION_CAP: u64 = 1_000;
        // An idle-tick period no test run approaches, so the tick arm fires
        // exactly once (immediately, before any command is queued).
        const HANDOFF_ONLY_TICK: Duration = Duration::from_secs(3600);

        let cfg = crate::config::EndpointConfig {
            udp_port: 0,
            advertised_addresses: vec!["::1".parse().expect("v6 literal")],
            ..crate::config::EndpointConfig::default()
        };
        let builder = UnifiedEndpoint::bind(cfg).expect("the single shared socket binds once");
        let bound = builder.local_addr().expect("a bound local addr");
        let (builder, whip) = builder.with_ingest();
        let endpoint = builder.build();

        // The ingest session's remote "publisher": a plain loopback socket
        // whose address the offer advertises as its host candidate. Once the
        // driver fires the session's ICE timer, str0m (full ICE) queues STUN
        // connectivity checks to it and the outbound pump sends them here —
        // an observable effect of the timer AND pump steps (PR193-02-R2).
        let peer = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            0,
            0,
            0,
        )))
        .expect("peer binds a v6 loopback");
        peer.set_nonblocking(true).expect("peer is non-blocking");
        let peer_addr = peer.local_addr().expect("peer addr");
        let bound_v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, bound.port(), 0, 0));

        let probe = Arc::new(RunProbe::default());
        let stop = Arc::new(AtomicBool::new(false));

        // Saturating flood from a dedicated OS thread (not a tokio worker) so
        // the driver's `socket.readable()` is effectively always ready — the
        // permanent, saturating, hostile flood the BLOCKER is about. Portable:
        // no platform-specific socket inspection sits on this path, only a
        // plain blocking `send_to` loop (review PR193-01).
        let flood_stop = Arc::new(AtomicBool::new(false));
        let mut flood_guard = FloodGuard {
            stop: Arc::clone(&stop),
            flood_stop: Arc::clone(&flood_stop),
            flooders: Vec::with_capacity(FLOODER_THREADS),
        };
        for _ in 0..FLOODER_THREADS {
            let flood_stop = Arc::clone(&flood_stop);
            flood_guard.flooders.push(std::thread::spawn(move || {
                let s = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::LOCALHOST,
                    0,
                    0,
                    0,
                )))
                .expect("flooder binds a v6 loopback");
                let target =
                    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, bound.port(), 0, 0));
                let junk = [0u8; 64];
                let deadline = Instant::now() + SAFETY_TIMEOUT;
                while !flood_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                    for _ in 0..1024 {
                        let _ = s.send_to(&junk, target);
                    }
                }
            }));
        }

        // The INDEPENDENT fairness observer: a separate task on the SAME
        // single-threaded runtime, genuinely contending with the driver for
        // the one worker thread. The fairness statistic is computed HERE, at
        // the observer's own polls (review PR193-03-R2): at each poll it reads
        // the driver's exhaustion count and records how many exhaustions ran
        // since ITS previous poll; the result is the max of those deltas. No
        // coordinator timer samples the counters, so scheduler/timer latency
        // cannot inflate the statistic, and the observer takes one final poll
        // after the driver has stopped, so a trailing starved interval with no
        // observer progress is counted too.
        let driver_done = Arc::new(AtomicBool::new(false));
        let observer = {
            let probe = Arc::clone(&probe);
            let driver_done = Arc::clone(&driver_done);
            tokio::spawn(async move {
                let mut last = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
                let mut max_gap = 0u64;
                let mut polls = 0u64;
                loop {
                    // Read `done` BEFORE the counter: once the driver has
                    // returned no further exhaustion can occur, so this poll's
                    // delta closes the final interval up to driver stop.
                    let done = driver_done.load(Ordering::Acquire);
                    let now = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
                    max_gap = max_gap.max(now - last);
                    last = now;
                    polls += 1;
                    if done {
                        return (max_gap, polls);
                    }
                    tokio::task::yield_now().await;
                }
            })
        };

        // Run the driver under the flood. `tokio::task::unconstrained` disables
        // Tokio's OWN cooperative-scheduling auto-yield (its periodic forced
        // yield after an I/O-bound poll budget) for this task, so the ONLY
        // thing that can give the single worker thread back to the observer is
        // the driver's own explicit `yield_now` on the exhaustion-handoff path.
        // Without this, Tokio's runtime-level fairness safety net would mask
        // removing that production `yield_now` (review PR193-03) — the
        // observer would still progress on Tokio's own budget resets, not on
        // anything the driver itself does.
        let driver = {
            let probe = Arc::clone(&probe);
            let stop = Arc::clone(&stop);
            let driver_done = Arc::clone(&driver_done);
            tokio::spawn(tokio::task::unconstrained(async move {
                // Idle tick far longer than the test: after the interval's
                // immediate first tick (which runs before any command is
                // queued), the exhaustion handoff is the ONLY servicing path,
                // so every effect asserted below is the handoff's.
                let result = endpoint
                    .run_inner_body(stop, &probe, MAX_DRAIN_TIME, HANDOFF_ONLY_TICK)
                    .await;
                driver_done.store(true, Ordering::Release);
                result
            }))
        };

        // Saturate first: wait (by count) for `TARGET_EXHAUSTIONS` exhausted
        // budgets AND at least one completed handoff, so every command below is
        // queued strictly AFTER the idle tick's one immediate firing and after
        // the flood has demonstrably reached the handoff path (PR193-02-R2).
        let saturated = tokio::time::timeout(SAFETY_TIMEOUT, async {
            while probe.recv_budgets_exhausted.load(Ordering::Relaxed) < TARGET_EXHAUSTIONS {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(
            saturated.is_ok(),
            "the flood must exhaust {TARGET_EXHAUSTIONS} receive budgets (hang \
             guard hit: the flood never saturated or the driver wedged)"
        );
        let first_handoff =
            within_exhaustions(&probe, SERVICE_EXHAUSTION_CAP, SAFETY_TIMEOUT, || {
                probe.handoff_services.load(Ordering::Relaxed) > 0
            })
            .await;
        assert!(
            first_handoff,
            "no exhaustion handoff completed within {SERVICE_EXHAUSTION_CAP} \
             exhausted budgets: the handoff is missing or returns early \
             (invariant #10 BLOCKER)"
        );

        // Mid-flood Register: ONLY a handoff can drain it now.
        let negotiated = whip
            .negotiate("flood-source", &publisher_offer(peer_addr), false)
            .expect("WHIP ingest negotiates a publisher mid-flood");
        let ring = negotiated.ring.clone();
        assert!(!ring.is_ended(), "freshly-negotiated ring is open");

        // TIMER + PUMP effect. Any STUN check at the peer proves the session
        // was registered (command drain) and its ICE timer fired (nothing else
        // makes str0m queue a check: the junk flood is never accepted). Which
        // servicing path SENT a received check is attributed at the source
        // (review PR193-07), not inferred from a time window: the driver logs
        // every Binding Request at its actual send site, tagged with the
        // servicing origin, destination and exact bytes. Each check the peer
        // receives is matched, in send order, to its own log entry; it counts
        // as the handoff's pump effect only if that entry is tagged
        // `ExhaustionHandoff` and addressed to the peer. Each queued datagram
        // is popped and sent exactly once, by exactly one path, so a match
        // has no other servicing path in between, and an unrelated dry drain
        // elsewhere can no longer discard it.
        let mut stun_seen = 0u64;
        let mut handoff_stun = 0u64;
        let mut other_stun = 0u64;
        // Received checks not yet matched (the send log is read after the
        // peer, so a check may briefly lead its own entry).
        let mut unmatched: Vec<Vec<u8>> = Vec::new();
        let mut consumed = vec![false; SENT_CHECK_LOG_CAP];
        let mut datagram = [0u8; 1500];
        let pumped = within_exhaustions(&probe, SERVICE_EXHAUSTION_CAP, SAFETY_TIMEOUT, || {
            while let Ok((len, _)) = peer.recv_from(&mut datagram) {
                if is_stun_binding_request(&datagram[..len]) {
                    stun_seen += 1;
                    unmatched.push(datagram[..len].to_vec());
                }
            }
            let log = probe.sent_checks.lock().expect("send log lock");
            unmatched.retain(|bytes| {
                let entry = log.iter().enumerate().find(|(i, (_, dst, sent))| {
                    !consumed[*i] && *dst == peer_addr && sent == bytes
                });
                match entry {
                    Some((i, (origin, _, _))) => {
                        consumed[i] = true;
                        if *origin == ServiceOrigin::ExhaustionHandoff {
                            handoff_stun += 1;
                        } else {
                            other_stun += 1;
                        }
                        false
                    }
                    None => true,
                }
            });
            handoff_stun > 0
        })
        .await;
        assert!(
            stun_seen > 0,
            "the ingest session's ICE timer never fired under the flood: no STUN \
             check reached the peer within {SERVICE_EXHAUSTION_CAP} exhausted \
             budgets (handoff timer step starved or deleted, PR193-02-R2)"
        );
        assert!(
            pumped,
            "{stun_seen} STUN checks reached the peer but none was sent by the \
             exhaustion handoff's own outbound pump ({other_stun} attributed to \
             another servicing path, {} unattributed) within \
             {SERVICE_EXHAUSTION_CAP} exhausted budgets (handoff outbound pump \
             starved or deleted, PR193-02-R2 / PR193-07)",
            unmatched.len()
        );

        // COMMAND-DRAIN effect: a mid-flood Release closes the ring only when
        // the handoff drains it.
        assert!(
            whip.release("flood-source", negotiated.session_id.as_str()),
            "release dispatches a teardown command for the live session"
        );
        let released = within_exhaustions(&probe, SERVICE_EXHAUSTION_CAP, SAFETY_TIMEOUT, || {
            ring.is_ended()
        })
        .await;
        assert!(
            released,
            "under a permanent flood the handoff must drain the Release command \
             (close the ring) within {SERVICE_EXHAUSTION_CAP} exhausted budgets \
             (handoff command drain starved or deleted, PR193-02-R2)"
        );

        // REAP/GC effect: only the driver's reap step closes the released
        // (now dead) session in the table; `release` itself only forgets the
        // source.
        let reaped = within_exhaustions(&probe, SERVICE_EXHAUSTION_CAP, SAFETY_TIMEOUT, || {
            whip.session_closed_in_table(&negotiated.session_id)
        })
        .await;
        assert!(
            reaped,
            "the handoff's reap/GC step must close the released session in the \
             table within {SERVICE_EXHAUSTION_CAP} exhausted budgets (handoff \
             reap starved or deleted, PR193-02-R2)"
        );

        // Keep the flood saturating while the observer measures, until the
        // window holds `FAIRNESS_WINDOW_EXHAUSTIONS` further exhaustions AND a
        // proven dry-free run of `REQUIRED_DRY_FREE_RUN` (or the count cap is
        // hit). This loop only decides WHEN to stop, by counts; it computes
        // nothing (the statistic lives in the observer). The window's dry-free
        // record starts fresh here so only a run inside the window counts.
        let exh_start = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
        probe.max_dry_free_run.store(0, Ordering::Relaxed);
        let window = tokio::time::timeout(SAFETY_TIMEOUT, async {
            loop {
                let done = probe.recv_budgets_exhausted.load(Ordering::Relaxed) - exh_start;
                let run = probe.max_dry_free_run.load(Ordering::Relaxed);
                if (done >= FAIRNESS_WINDOW_EXHAUSTIONS && run >= REQUIRED_DRY_FREE_RUN)
                    || done >= FAIRNESS_EXHAUSTION_CAP
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        let window_run = probe.max_dry_free_run.load(Ordering::Relaxed);

        stop.store(true, Ordering::Release);
        // The idle tick is an hour away, so wake the driver with one datagram
        // in case the flood has already ended: it must reach its top-of-loop
        // stop check, not park on an empty socket.
        let _ = peer.send_to(b"wake", bound_v6);
        let stopped = tokio::time::timeout(SAFETY_TIMEOUT, driver).await;
        // Hang guard only: the observer returns once it sees the driver stop.
        let (max_gap, observer_polls) = tokio::time::timeout(SAFETY_TIMEOUT, observer)
            .await
            .expect("the observer finishes once the driver has stopped")
            .expect("the observer task completes");
        // Stop + join the flooder now (the guard also does this on any panic
        // path above, review PR193-05).
        drop(flood_guard);

        let exhausted = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
        let handoffs = probe.handoff_services.load(Ordering::Relaxed);

        assert!(
            exhausted >= TARGET_EXHAUSTIONS,
            "the flood must exhaust at least {TARGET_EXHAUSTIONS} receive budgets \
             (got {exhausted}); the flood was not saturating"
        );
        // The discriminating ordering invariant: the explicit HANDOFF (not idle
        // ticks) fires exactly once per exhausted budget, and is only bumped
        // AFTER every real servicing step ran.
        assert_eq!(
            handoffs, exhausted,
            "the explicit exhaustion handoff must service exactly once per \
             exhausted receive budget, independent of idle-tick servicing: \
             {handoffs} handoffs != {exhausted} exhausted budgets — the rest of \
             the driver was starved (invariant #10 BLOCKER)"
        );
        assert!(
            window.is_ok(),
            "hang guard: the fairness window did not reach its exhaustion \
             counts within {SAFETY_TIMEOUT:?}"
        );
        assert!(
            window_run >= REQUIRED_DRY_FREE_RUN,
            "saturation precondition (review PR193-06): the measured window must \
             contain {REQUIRED_DRY_FREE_RUN} consecutive exhausted budgets with \
             no drain reaching WouldBlock, but the longest was {window_run} — \
             the flood was not continuously readable, so a natural yield could \
             mask a missing production `yield_now`"
        );
        assert!(
            observer_polls > 1,
            "the observer must have polled during the run (got {observer_polls})"
        );
        assert!(
            max_gap <= MAX_TOLERATED_GAP,
            "an independent task on the same (single-threaded) runtime must get \
             scheduled at least every {MAX_TOLERATED_GAP} exhaustions while the \
             driver services a permanent flood, but it went {max_gap} \
             exhaustions without a scheduling chance — the driver is not \
             yielding the thread between receive budgets (review PR193-03)"
        );
        assert!(
            matches!(stopped, Ok(Ok(Ok(())))),
            "the driver must honour stop within the bound under flood, got {stopped:?}"
        );
    }

    /// Invariant #10 / MAJOR #1: a flood of inbound datagrams CANNOT keep the
    /// driver in the inbound drain indefinitely. One `drain_inbound_budgeted` wake
    /// reads **at most** the budget and reports `budget_exhausted`, so it breaks to
    /// pump/yield instead of looping until the (never-arriving under flood)
    /// `WouldBlock`. Driven against a REAL loopback socket flooded well past the
    /// budget — deterministic, fast, kernel-backed.
    #[tokio::test]
    async fn an_inbound_flood_cannot_starve_the_driver() {
        // A real dual-stack loopback receiver with a large receive buffer so the
        // whole flood is queued (the test asserts the *budget* caps the drain, not
        // the OS buffer).
        let recv = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        recv.set_only_v6(false).unwrap();
        // Big enough for >> MAX_DATAGRAMS_PER_WAKE small datagrams.
        let _ = recv.set_recv_buffer_size(8 * 1024 * 1024);
        recv.set_nonblocking(true).unwrap();
        let bind = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0));
        recv.bind(&bind.into()).unwrap();
        let local_addr = recv.local_addr().unwrap().as_socket().unwrap();
        crate::transport::local_addr::enable_pktinfo(&recv).unwrap();

        let recv = tokio::net::UdpSocket::from_std(recv.into()).unwrap();

        // Flood: send well past the per-wake budget so a naive unbounded drain
        // would read them all in one go (and, under a real sustained flood, never
        // stop).
        let flood = MAX_DATAGRAMS_PER_WAKE * 3;
        let sender = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            0,
            0,
            0,
        )))
        .unwrap();
        let mut sent = 0usize;
        for _ in 0..flood {
            if sender.send_to(b"flood", local_addr).is_ok() {
                sent += 1;
            }
        }
        assert!(
            sent > MAX_DATAGRAMS_PER_WAKE,
            "test precondition: queued {sent} datagrams (> budget {MAX_DATAGRAMS_PER_WAKE})"
        );
        // Sending does not imply that the receive queue already holds a full
        // budget: wait for the *queued* datagrams, not just one readable edge.
        #[cfg(target_os = "linux")]
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if queued_datagrams(&recv).unwrap() > MAX_DATAGRAMS_PER_WAKE {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("more than one receive budget must be queued before draining");
        // Register Tokio's readiness before try_io; counting with the raw socket
        // alone does not arm its reactor. Non-Linux retains the old precondition.
        recv.readable().await.unwrap();

        let mut buf = vec![0u8; RECV_BUFFER];
        let concrete = [local_addr];

        // ONE wake. A bounded drain returns promptly; an unbounded one would keep
        // reading every queued datagram (and spin forever on a live flood).
        let mut routed = 0usize;
        let started = Instant::now();
        let outcome = UnifiedEndpoint::drain_inbound_budgeted_with(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            Duration::from_secs(10), // isolate the count cap from host scheduling
            |_src, _dst, _payload| {
                routed += 1;
            },
        );
        let elapsed = started.elapsed();

        // The drain stopped at the budget, NOT at the socket draining.
        assert!(
            outcome.budget_exhausted,
            "the flood must trip the per-wake budget (so the driver yields), \
             but the drain ran to WouldBlock: {outcome:?}"
        );
        assert_eq!(
            outcome.read_count, MAX_DATAGRAMS_PER_WAKE,
            "exactly the count budget is read before breaking to pump/yield"
        );
        assert_eq!(
            routed, MAX_DATAGRAMS_PER_WAKE,
            "every budgeted datagram was routed (none dropped beyond the cap)"
        );
        // Bounded work => fast; never the unbounded spin the panel flagged. Very
        // generous ceiling to stay non-flaky on a loaded CI box.
        assert!(
            elapsed < Duration::from_secs(1),
            "one budgeted wake must be bounded/fast, took {elapsed:?}"
        );

        // The deferred datagrams are NOT lost — a second wake drains more,
        // proving the budget defers rather than drops (the OS buffer held them).
        let mut routed2 = 0usize;
        let outcome2 = UnifiedEndpoint::drain_inbound_budgeted(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            |_src, _dst, _payload| {
                routed2 += 1;
            },
        );
        assert!(
            routed2 > 0 && outcome2.read_count > 0,
            "the next wake drains the datagrams deferred by the budget: {outcome2:?}"
        );
    }

    #[tokio::test]
    async fn inbound_time_budget_defers_queued_datagrams() {
        let recv = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        recv.set_only_v6(false).unwrap();
        recv.set_nonblocking(true).unwrap();
        let bind = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0));
        recv.bind(&bind.into()).unwrap();
        let local_addr = recv.local_addr().unwrap().as_socket().unwrap();
        crate::transport::local_addr::enable_pktinfo(&recv).unwrap();
        let recv = tokio::net::UdpSocket::from_std(recv.into()).unwrap();
        let sender = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            0,
            0,
            0,
        )))
        .unwrap();
        sender.send_to(b"first", local_addr).unwrap();
        sender.send_to(b"second", local_addr).unwrap();
        recv.readable().await.unwrap();

        let mut buf = vec![0u8; RECV_BUFFER];
        let concrete = [local_addr];
        let outcome = UnifiedEndpoint::drain_inbound_budgeted_with(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            Duration::ZERO,
            |_src, _dst, _payload| {},
        );
        assert!(outcome.budget_exhausted, "time budget must stop the drain");
        assert!(outcome.read_count < MAX_DATAGRAMS_PER_WAKE);
        let next = UnifiedEndpoint::drain_inbound_budgeted_with(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            Duration::from_secs(10),
            |_src, _dst, _payload| {},
        );
        assert!(
            next.read_count > 0,
            "the next wake drains deferred datagrams: {next:?}"
        );
    }

    /// A drain over an EMPTY socket reports the socket drained (`WouldBlock`),
    /// reads nothing, and does NOT claim the budget was exhausted — so the driver
    /// parks on its timer rather than busy-looping (the no-flood steady state).
    #[tokio::test]
    async fn an_empty_socket_drains_without_exhausting_the_budget() {
        let recv = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        recv.set_only_v6(false).unwrap();
        recv.set_nonblocking(true).unwrap();
        let bind = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0));
        recv.bind(&bind.into()).unwrap();
        let local_addr = recv.local_addr().unwrap().as_socket().unwrap();
        crate::transport::local_addr::enable_pktinfo(&recv).unwrap();
        let recv = tokio::net::UdpSocket::from_std(recv.into()).unwrap();

        let mut buf = vec![0u8; RECV_BUFFER];
        let concrete = [local_addr];
        let mut routed = 0usize;
        let outcome = UnifiedEndpoint::drain_inbound_budgeted(
            &recv,
            &mut buf,
            local_addr,
            &concrete,
            |_s, _d, _p| routed += 1,
        );
        assert_eq!(routed, 0, "no datagrams queued => nothing routed");
        assert_eq!(outcome.read_count, 0);
        assert!(
            !outcome.budget_exhausted,
            "an empty socket drains to WouldBlock, it does not exhaust the budget"
        );
    }

    /// A shared UDP lane that is ALWAYS ready: an infinite stream of garbage
    /// datagrams (review I1, PR 337).
    struct EndlessUdp;

    impl SharedInbound for EndlessUdp {
        fn poll_next(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<InboundDatagram>> {
            std::task::Poll::Ready(self.try_next())
        }
        fn try_next(&mut self) -> Option<InboundDatagram> {
            Some(InboundDatagram {
                src: "[2001:db8::99]:4000".parse().expect("v6 literal"),
                dst_ip: Some(Ipv6Addr::LOCALHOST.into()),
                payload: vec![0xAB; 64],
            })
        }
        fn validated(&mut self, _peer: SocketAddr) -> bool {
            false
        }
        fn invalidated(&mut self, _peer: SocketAddr) {}
    }

    /// An ICE-TCP lane holding a finite number of frames; counts consumption.
    struct CountingTcp {
        queued: std::collections::VecDeque<InboundFrame>,
        consumed: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TcpSend for CountingTcp {
        fn send(&self, _peer: SocketAddr, _payload: &[u8]) -> bool {
            false
        }
    }

    impl SharedTcp for CountingTcp {
        fn poll_next(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<InboundFrame>> {
            // Never becomes ready again once empty: no waker is needed.
            match self.try_next() {
                Some(f) => std::task::Poll::Ready(Some(f)),
                None => std::task::Poll::Pending,
            }
        }
        fn try_next(&mut self) -> Option<InboundFrame> {
            let f = self.queued.pop_front()?;
            self.consumed.fetch_add(1, Ordering::Relaxed);
            Some(f)
        }
    }

    /// Review I1 (PR 337): with the shared UDP lane permanently ready, the
    /// biased `select!` alone never reaches the TCP arm. The cross-lane drain
    /// after an exhausted budget must still consume every TCP frame, and the
    /// idle tick must keep firing.
    #[tokio::test(flavor = "current_thread")]
    async fn a_saturated_udp_lane_cannot_starve_the_tcp_lane_or_the_tick() {
        const FRAMES: usize = 3 * MAX_DATAGRAMS_PER_WAKE + 17;
        const MIN_TICKS: u64 = 5;
        let socket = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::UNSPECIFIED,
            0,
            0,
            0,
        )))
        .expect("dual-stack bind");
        let port = socket.local_addr().expect("local addr").port();
        let cfg = crate::config::EndpointConfig {
            udp_port: port,
            advertised_addresses: vec![Ipv6Addr::LOCALHOST.into()],
            ice_tcp_port: Some(port),
            ..crate::config::EndpointConfig::default()
        };
        let endpoint = UnifiedEndpoint::with_shared_socket(cfg, socket)
            .expect("shared endpoint")
            .build();
        let local: SocketAddr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port);
        let consumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tcp = CountingTcp {
            queued: (0..FRAMES)
                .map(|_| InboundFrame {
                    peer: "192.0.2.7:5000".parse().expect("v4 literal"),
                    local,
                    payload: vec![0xCD; 32],
                })
                .collect(),
            consumed: Arc::clone(&consumed),
        };
        let probe = Arc::new(RunProbe::default());
        let stop = Arc::new(AtomicBool::new(false));
        let driver = {
            let probe = Arc::clone(&probe);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                endpoint
                    .run_shared_body(EndlessUdp, Some(tcp), stop, &probe)
                    .await
            })
        };
        let done = tokio::time::timeout(Duration::from_secs(20), async {
            while consumed.load(Ordering::Relaxed) < FRAMES
                || probe.idle_tick_services.load(Ordering::Relaxed) < MIN_TICKS
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        stop.store(true, Ordering::Release);
        let exhausted = probe.recv_budgets_exhausted.load(Ordering::Relaxed);
        let ticks = probe.idle_tick_services.load(Ordering::Relaxed);
        let got = consumed.load(Ordering::Relaxed);
        assert!(
            done.is_ok(),
            "within the bound: {got}/{FRAMES} TCP frames consumed, {ticks} idle ticks \
             (need {MIN_TICKS}), {exhausted} exhausted budgets"
        );
        assert!(exhausted > 0, "the UDP lane must have been saturating");
        driver
            .await
            .expect("driver task joins")
            .expect("driver exits cleanly");
    }
}
