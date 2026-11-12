//! The network front door, slice 1 (ADR-0107, design note
//! `docs/architecture/network-front-door.md` sections 2, 3 and 13; issue #287).
//!
//! One dual-stack `[::]` UDP socket carries QUIC (`MoQ` over WebTransport) and
//! WebRTC (STUN, DTLS, SRTP/SRTCP, TURN). A single dispatcher task reads it in
//! recvmmsg/GRO batches, classifies every datagram by its first byte (RFC 9443
//! section 3, [`classify`]), and hands it to the QUIC lane or the WebRTC lane
//! through bounded queues with `try_send` (full = drop and count). Sends bypass
//! the dispatcher and go straight to the socket.
//!
//! ## Runtime topology (invariant #1)
//!
//! The front door owns its **own** multi-thread Tokio runtime
//! ([`FrontDoorConfig::workers`] OS threads named `frontdoor-N`). The socket is
//! registered with that runtime's reactor; the dispatcher, the noq endpoint
//! driver, the `MoQ` accept loop and the WebRTC driver all run there. Nothing in
//! this crate spawns onto the caller's runtime, and nothing here shares a lock or
//! a queue with the program runtime that drives the output clock. The only state
//! shared across tasks is the [`EstablishedSet`] (read with `try_read`, never
//! blocking) and the lane queues (bounded, `try_send`).
//!
//! ## Bounded state (invariant #10)
//!
//! Per lane: an *established* queue (1024) for validated peers and a *new*
//! queue (256) behind a per-source token bucket (IPv4 address or IPv6 /64, in a
//! 16 384-entry LRU) and a global new-flow budget; unsupported-version long
//! headers (Version Negotiation triggers) also pay a global VN budget,
//! supported-version Initials a global handshake budget (from any source,
//! established or not), and STUN from unvalidated sources its own per-source
//! and global STUN budgets. Established peers pass a generous per-peer bucket.
//! The established set is written only by the consumers after validation and
//! is capped by the session/connection caps. Nothing grows with attacker
//! traffic.
//!
//! ## Validated-flow steering (slice 3, Linux)
//!
//! With [`FrontDoorConfig::steering`] the door binds a `SO_REUSEPORT` pair on
//! the same port: a listener (all unvalidated traffic, and every send) and an
//! established socket that a classic BPF program ([`steering`]) feeds with the
//! datagrams of validated peers, so a flood fills the listener's kernel queue,
//! never the paying viewers'. A steering updater task recompiles the program
//! off the data plane when an established set changes; the dispatcher never
//! compiles or attaches. Without Linux, or on any setup error, the door runs
//! one socket ([`SteeringMode::SingleSocket`]).

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod classify;
pub mod flood;
pub mod limits;
pub mod quic;
pub mod rtc;
mod socket;
pub mod steering;
#[cfg(target_os = "linux")]
mod sys;

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

pub use classify::{classify, first_byte_class, ByteClass, DropReason, Lane, QuicKind, Verdict};
pub use limits::{Budget, EstablishedSet, NewFlowLimiter, NewFlowLimits, PeerBuckets, Refusal};
pub use quic::QuicLane;
pub use rtc::RtcLane;
pub use socket::{
    kernel_drops, kernel_drops_supported, parse_udp_drops, RecvBufferReport, SharedSocket,
};
pub use steering::{ProgramSet, ReuseportGroup, SteeringMode, STEERING_MAX_PEERS};

/// Established-queue depth per lane (design note section 13).
pub const ESTABLISHED_QUEUE: usize = 1024;
/// New-flow-queue depth per lane (design note section 13).
pub const NEW_QUEUE: usize = 256;
/// How long [`FrontDoor::shutdown_blocking`] waits for the runtime's tasks.
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Default cap on established QUIC peers (`network.quic.max_connections`).
pub const DEFAULT_MAX_QUIC_PEERS: usize = 1024;
/// How long the steering updater waits after a change before compiling, so a
/// burst of joins becomes one program (design: at most 20 ms).
pub const STEERING_DEBOUNCE: Duration = Duration::from_millis(20);
/// How often the kernel-drop sampler reads `/proc/net/udp6`.
pub const KERNEL_DROP_SAMPLE: Duration = Duration::from_secs(1);
/// Least interval between two kernel-drop (or steering-error) warnings.
const WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Front-door settings (the `[network]` section, slice 1 subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontDoorConfig {
    /// The UDP port, bound on `[::]` dual-stack. `0` picks an ephemeral port
    /// (tests).
    pub port: u16,
    /// Worker threads of the front door's own runtime (at least 1).
    pub workers: usize,
    /// `SO_RCVBUF` request; the kernel may clamp it (`rmem_max`), which is
    /// logged.
    pub recv_buffer_bytes: usize,
    /// New-flow limits.
    pub limits: NewFlowLimits,
    /// Cap on established WebRTC peers (the WebRTC session caps).
    pub max_rtc_peers: usize,
    /// Cap on established QUIC peers (`network.quic.max_connections`).
    pub max_quic_peers: usize,
    /// TURN servers the WebRTC endpoint allocates on. Only datagrams from these
    /// addresses may be TURN `ChannelData` (first byte 64-79). They are also
    /// steered to the established socket (validated by configuration).
    pub turn_servers: Vec<SocketAddr>,
    /// Validated-flow steering (Linux `SO_REUSEPORT` + CBPF). `false` forces
    /// one socket. Default `true`; falls back to one socket where it cannot
    /// run (see [`FrontDoor::steering`]).
    pub steering: bool,
}

impl Default for FrontDoorConfig {
    fn default() -> Self {
        Self {
            port: 443,
            workers: 2,
            recv_buffer_bytes: 8 * 1024 * 1024,
            limits: NewFlowLimits::default(),
            max_rtc_peers: 256,
            max_quic_peers: DEFAULT_MAX_QUIC_PEERS,
            turn_servers: Vec::new(),
            steering: true,
        }
    }
}

/// One received datagram handed to a lane.
#[derive(Debug, Clone)]
pub struct Datagram {
    /// The remote address as the dual-stack socket reported it (IPv4 peers as
    /// `::ffff:a.b.c.d`).
    pub src: SocketAddr,
    /// The concrete local IP the datagram arrived on (PKTINFO), IPv4-mapped
    /// addresses unmapped. `None` if the kernel reported none.
    pub dst_ip: Option<IpAddr>,
    /// ECN codepoint bits, as `noq::udp` reported them.
    pub ecn: Option<noq::udp::EcnCodepoint>,
    /// The datagram.
    pub payload: Vec<u8>,
}

/// Counters, read wait-free by anyone (telemetry, tests).
#[derive(Debug, Default)]
pub struct Stats {
    /// Datagrams read from the socket (after GRO splitting).
    pub received: AtomicU64,
    /// Delivered to the QUIC lane.
    pub to_quic: AtomicU64,
    /// Delivered to the WebRTC lane.
    pub to_rtc: AtomicU64,
    /// Dropped by the classifier (malformed, reserved, Retry, VN, short Initial).
    pub dropped_classify: AtomicU64,
    /// Dropped by the per-source bucket.
    pub dropped_per_source: AtomicU64,
    /// Dropped by the global new-flow budget.
    pub dropped_global: AtomicU64,
    /// Dropped by the global Version Negotiation budget.
    pub dropped_vn: AtomicU64,
    /// Dropped by the global handshake (Initial) budget.
    pub dropped_initial: AtomicU64,
    /// Dropped because a lane queue was full.
    pub dropped_queue_full: AtomicU64,
    /// Dropped because the lane had no consumer.
    pub dropped_no_consumer: AtomicU64,
    /// Socket receive errors (other than `WouldBlock`).
    pub recv_errors: AtomicU64,
    /// Source keys currently tracked by the new-flow limiter.
    pub tracked_sources: AtomicU64,
    /// Datagrams read from the established socket (steered peers).
    pub received_established_socket: AtomicU64,
    /// Datagrams read from the listener socket.
    pub received_listener_socket: AtomicU64,
    /// Datagrams read from the established socket whose source is in no
    /// established set (a hash collision, or a peer that just left): they
    /// took the new-flow path like any other new flow.
    pub steered_unverified: AtomicU64,
    /// Gauge: established peers not steered because of [`STEERING_MAX_PEERS`].
    pub steering_overflow: AtomicU64,
    /// Steering programs attached.
    pub steering_updates: AtomicU64,
    /// Steering attaches that failed (the previous program stays).
    pub steering_update_errors: AtomicU64,
    /// Gauge: peers the current program steers.
    pub steered_peers: AtomicU64,
    /// Gauge: kernel receive drops on the listener socket (`/proc/net/udp6`).
    pub kernel_drops_listener: AtomicU64,
    /// Gauge: kernel receive drops on the established socket.
    pub kernel_drops_established: AtomicU64,
    /// Dropped by the per-source STUN bucket.
    pub dropped_stun_per_source: AtomicU64,
    /// Dropped by the global STUN budget.
    pub dropped_stun_global: AtomicU64,
    /// Dropped by the per-established-peer bucket (issue #325).
    pub dropped_established_peer: AtomicU64,
}

impl Stats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A plain copy of every counter.
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        StatsSnapshot {
            received: get(&self.received),
            to_quic: get(&self.to_quic),
            to_rtc: get(&self.to_rtc),
            dropped_classify: get(&self.dropped_classify),
            dropped_per_source: get(&self.dropped_per_source),
            dropped_global: get(&self.dropped_global),
            dropped_vn: get(&self.dropped_vn),
            dropped_initial: get(&self.dropped_initial),
            dropped_queue_full: get(&self.dropped_queue_full),
            dropped_no_consumer: get(&self.dropped_no_consumer),
            recv_errors: get(&self.recv_errors),
            tracked_sources: get(&self.tracked_sources),
            received_established_socket: get(&self.received_established_socket),
            received_listener_socket: get(&self.received_listener_socket),
            steered_unverified: get(&self.steered_unverified),
            steering_overflow: get(&self.steering_overflow),
            steering_updates: get(&self.steering_updates),
            steering_update_errors: get(&self.steering_update_errors),
            steered_peers: get(&self.steered_peers),
            kernel_drops_listener: get(&self.kernel_drops_listener),
            kernel_drops_established: get(&self.kernel_drops_established),
            dropped_stun_per_source: get(&self.dropped_stun_per_source),
            dropped_stun_global: get(&self.dropped_stun_global),
            dropped_established_peer: get(&self.dropped_established_peer),
        }
    }
}

/// A copy of [`Stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct StatsSnapshot {
    pub received: u64,
    pub to_quic: u64,
    pub to_rtc: u64,
    pub dropped_classify: u64,
    pub dropped_per_source: u64,
    pub dropped_global: u64,
    pub dropped_vn: u64,
    pub dropped_initial: u64,
    pub dropped_queue_full: u64,
    pub dropped_no_consumer: u64,
    pub recv_errors: u64,
    pub tracked_sources: u64,
    pub received_established_socket: u64,
    pub received_listener_socket: u64,
    pub steered_unverified: u64,
    pub steering_overflow: u64,
    pub steering_updates: u64,
    pub steering_update_errors: u64,
    pub steered_peers: u64,
    pub kernel_drops_listener: u64,
    pub kernel_drops_established: u64,
    pub dropped_stun_per_source: u64,
    pub dropped_stun_global: u64,
    pub dropped_established_peer: u64,
}

/// Errors starting the front door.
#[derive(Debug)]
pub enum Error {
    /// The runtime could not be built.
    Runtime(std::io::Error),
    /// The socket could not be created, configured or bound.
    Socket {
        /// The address that was being bound.
        addr: SocketAddr,
        /// The OS error.
        source: std::io::Error,
    },
    /// A lane was taken twice.
    LaneTaken(Lane),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "front-door runtime: {e}"),
            Self::Socket { addr, source } => write!(f, "front-door socket {addr}: {source}"),
            Self::LaneTaken(lane) => write!(f, "front-door {lane:?} lane already taken"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(e) | Self::Socket { source: e, .. } => Some(e),
            Self::LaneTaken(_) => None,
        }
    }
}

/// The receiving ends of one lane: established first, then new.
#[derive(Debug)]
pub struct LaneRx {
    established: mpsc::Receiver<Datagram>,
    new: mpsc::Receiver<Datagram>,
}

impl LaneRx {
    /// Wait for the next datagram, established flows first. `None` once the
    /// dispatcher is gone.
    pub async fn recv(&mut self) -> Option<Datagram> {
        tokio::select! {
            biased;
            Some(d) = self.established.recv() => Some(d),
            Some(d) = self.new.recv() => Some(d),
            else => None,
        }
    }

    /// Take a queued datagram without waiting, established flows first.
    pub fn try_recv(&mut self) -> Option<Datagram> {
        self.established
            .try_recv()
            .ok()
            .or_else(|| self.new.try_recv().ok())
    }

    /// Poll for the next datagram, established flows first.
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Datagram>> {
        use std::task::Poll;
        let established = self.established.poll_recv(cx);
        if let Poll::Ready(Some(d)) = established {
            return Poll::Ready(Some(d));
        }
        match self.new.poll_recv(cx) {
            Poll::Ready(Some(d)) => Poll::Ready(Some(d)),
            Poll::Ready(None) if matches!(established, Poll::Ready(None)) => Poll::Ready(None),
            _ => Poll::Pending,
        }
    }
}

#[derive(Debug)]
struct LaneTx {
    established: mpsc::Sender<Datagram>,
    new: mpsc::Sender<Datagram>,
}

fn lane_channel() -> (LaneTx, LaneRx) {
    let (est_tx, est_rx) = mpsc::channel(ESTABLISHED_QUEUE);
    let (new_tx, new_rx) = mpsc::channel(NEW_QUEUE);
    (
        LaneTx {
            established: est_tx,
            new: new_tx,
        },
        LaneRx {
            established: est_rx,
            new: new_rx,
        },
    )
}

/// The front door: its runtime, the shared socket, and the two lanes waiting to
/// be taken by their consumers.
///
/// Dropping it shuts the runtime down in the background (never blocks the
/// caller, whatever runtime it is on).
pub struct FrontDoor {
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
    socket: Arc<SharedSocket>,
    stats: Arc<Stats>,
    quic_established: Arc<EstablishedSet>,
    rtc_established: Arc<EstablishedSet>,
    quic_rx: Option<LaneRx>,
    rtc_rx: Option<LaneRx>,
}

impl std::fmt::Debug for FrontDoor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontDoor")
            .field("local_addr", &self.socket.local_addr())
            .finish_non_exhaustive()
    }
}

impl FrontDoor {
    /// Build the runtime, bind `[::]:port` dual-stack on it and start the
    /// dispatcher. Never binds `0.0.0.0`.
    ///
    /// Call from anywhere (it does not need, or touch, the caller's runtime).
    ///
    /// # Errors
    ///
    /// [`Error::Runtime`] or [`Error::Socket`].
    pub fn start(config: &FrontDoorConfig) -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.workers.max(1))
            .thread_name_fn(|| {
                static N: AtomicU64 = AtomicU64::new(0);
                format!("frontdoor-{}", N.fetch_add(1, Ordering::Relaxed))
            })
            .enable_all()
            .build()
            .map_err(Error::Runtime)?;
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), config.port);
        let turn_servers: Vec<SocketAddr> = config
            .turn_servers
            .iter()
            .copied()
            .map(classify::canonical)
            .collect();
        let socket = {
            // The tokio socket registers with the reactor of the runtime it is
            // created in: the front door's, never the caller's.
            let _guard = runtime.enter();
            bind_socket(addr, config, &turn_servers)?
        };
        let socket = Arc::new(socket);
        let stats = Arc::new(Stats::default());
        let quic_established = Arc::new(EstablishedSet::new(config.max_quic_peers));
        let rtc_established = Arc::new(EstablishedSet::new(config.max_rtc_peers));
        let (quic_tx, quic_rx) = lane_channel();
        let (rtc_tx, rtc_rx) = lane_channel();
        let now = Instant::now();
        let dispatcher = Dispatcher {
            socket: Arc::clone(&socket),
            stats: Arc::clone(&stats),
            limiter: NewFlowLimiter::new(config.limits, now),
            peers: PeerBuckets::new(
                &config.limits,
                config
                    .max_quic_peers
                    .saturating_add(config.max_rtc_peers)
                    .max(1),
            ),
            peers_generation: 0,
            quic: quic_tx,
            rtc: rtc_tx,
            quic_established: Arc::clone(&quic_established),
            rtc_established: Arc::clone(&rtc_established),
            turn_servers: turn_servers.clone(),
        };
        runtime.spawn(dispatcher.run());
        if socket.has_established() {
            stats.steering_updates.fetch_add(1, Ordering::Relaxed);
            runtime.spawn(steering_updater(
                Arc::clone(&socket),
                Arc::clone(&stats),
                Arc::clone(&quic_established),
                Arc::clone(&rtc_established),
                turn_servers,
            ));
        }
        if kernel_drops_supported() {
            runtime.spawn(kernel_drop_sampler(Arc::clone(&socket), Arc::clone(&stats)));
        }
        log_mode(&socket, config);
        Ok(Self {
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            socket,
            stats,
            quic_established,
            rtc_established,
            quic_rx: Some(quic_rx),
            rtc_rx: Some(rtc_rx),
        })
    }

    /// The bound address (`[::]:port`).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// How the front door receives: the steered pair or one socket (and why).
    #[must_use]
    pub fn steering(&self) -> SteeringMode {
        self.socket.mode().clone()
    }

    /// The receive buffer the kernel granted (the smaller of the pair's).
    #[must_use]
    pub fn buffer_report(&self) -> RecvBufferReport {
        self.socket.buffer_report()
    }

    /// Whether the kernel-drop gauges are sampled on this platform.
    #[must_use]
    pub const fn kernel_drops_supported(&self) -> bool {
        kernel_drops_supported()
    }

    /// A handle to the front door's own runtime. Spawn every task that serves
    /// this socket here, never on the program runtime.
    ///
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.handle.clone()
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// The validated QUIC peers (written by the QUIC consumer after a
    /// confirmed handshake).
    #[must_use]
    pub fn quic_established(&self) -> Arc<EstablishedSet> {
        Arc::clone(&self.quic_established)
    }

    /// The validated WebRTC peers (written by the WebRTC consumer when ICE
    /// connects).
    #[must_use]
    pub fn rtc_established(&self) -> Arc<EstablishedSet> {
        Arc::clone(&self.rtc_established)
    }

    /// Take the QUIC lane: an `AsyncUdpSocket` for `noq::Endpoint`.
    ///
    /// # Errors
    ///
    /// [`Error::LaneTaken`] on a second call.
    pub fn take_quic_lane(&mut self) -> Result<QuicLane, Error> {
        let rx = self.quic_rx.take().ok_or(Error::LaneTaken(Lane::Quic))?;
        Ok(QuicLane::new(rx, Arc::clone(&self.socket)))
    }

    /// Take the WebRTC lane.
    ///
    /// # Errors
    ///
    /// [`Error::LaneTaken`] on a second call.
    pub fn take_rtc_lane(&mut self) -> Result<RtcLane, Error> {
        let rx = self.rtc_rx.take().ok_or(Error::LaneTaken(Lane::Rtc))?;
        Ok(RtcLane::new(
            rx,
            Arc::clone(&self.socket),
            Arc::clone(&self.rtc_established),
        ))
    }
}

impl FrontDoor {
    /// Stop the runtime and wait (at most [`SHUTDOWN_TIMEOUT`]) for its tasks,
    /// which releases the socket. Blocks: call it from a plain thread or
    /// `spawn_blocking`, never from inside an async task.
    pub fn shutdown_blocking(mut self) {
        // The lanes not taken by a consumer hold no socket; drop them first.
        self.quic_rx = None;
        self.rtc_rx = None;
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(SHUTDOWN_TIMEOUT);
        }
    }
}

impl Drop for FrontDoor {
    /// Dropping without [`shutdown_blocking`](Self::shutdown_blocking) never
    /// blocks the caller (whatever runtime it is on): the runtime shuts down in
    /// the background, so the socket closes shortly **after** the drop returns.
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

/// Bind the door's socket(s) in the current runtime: the steered pair where
/// possible (first program attached), else one socket with the reason.
fn bind_socket(
    addr: SocketAddr,
    config: &FrontDoorConfig,
    turn_servers: &[SocketAddr],
) -> Result<SharedSocket, Error> {
    let steering = config.steering && cfg!(target_os = "linux");
    let mut socket = SharedSocket::bind(addr, config.recv_buffer_bytes, steering)
        .map_err(|source| Error::Socket { addr, source })?;
    if socket.has_established() {
        // The first attach decides the mode: on failure, close the pair and
        // run one socket on the same port.
        let first = ProgramSet::build(steering::random_salt(), turn_servers.iter().copied());
        if let Err(e) = socket.attach(&first) {
            let port = socket.local_addr().port();
            let reason = format!("SO_ATTACH_REUSEPORT_CBPF failed: {e}");
            drop(socket);
            let addr = SocketAddr::new(addr.ip(), port);
            socket = SharedSocket::bind(addr, config.recv_buffer_bytes, false)
                .map_err(|source| Error::Socket { addr, source })?
                .with_reason(reason);
        }
    } else if config.steering && !cfg!(target_os = "linux") {
        socket = socket.with_reason(
            "validated-flow steering is Linux only (one kernel queue on this platform)".to_owned(),
        );
    }
    Ok(socket)
}

/// Log the receive mode at start: info when steering is active (or off by
/// configuration / platform), warn when it fell back on Linux.
fn log_mode(socket: &SharedSocket, config: &FrontDoorConfig) {
    let buffers = socket.buffer_report();
    match socket.mode() {
        SteeringMode::ReuseportCbpf => tracing::info!(
            local = %socket.local_addr(),
            workers = config.workers.max(1),
            rcvbuf_usable = buffers.usable,
            "front door: UDP for QUIC and WebRTC on its own runtime, validated flows \
             steered to their own socket (SO_REUSEPORT + CBPF)"
        ),
        SteeringMode::SingleSocket { reason } if config.steering && cfg!(target_os = "linux") => {
            tracing::warn!(
                local = %socket.local_addr(),
                workers = config.workers.max(1),
                rcvbuf_usable = buffers.usable,
                %reason,
                "front door: one UDP socket (validated-flow steering unavailable): a \
                 flood shares the kernel queue with established viewers"
            );
        }
        SteeringMode::SingleSocket { reason } => tracing::info!(
            local = %socket.local_addr(),
            workers = config.workers.max(1),
            rcvbuf_usable = buffers.usable,
            %reason,
            "front door: one UDP socket for QUIC and WebRTC on its own runtime"
        ),
    }
}

/// The one task that reads the socket(s) and feeds the lanes. It alone owns
/// the limiter and the per-peer buckets (no lock).
struct Dispatcher {
    socket: Arc<SharedSocket>,
    stats: Arc<Stats>,
    limiter: NewFlowLimiter,
    peers: PeerBuckets,
    /// Sum of both established sets' generations when `peers` was last pruned.
    peers_generation: u64,
    quic: LaneTx,
    rtc: LaneTx,
    quic_established: Arc<EstablishedSet>,
    rtc_established: Arc<EstablishedSet>,
    turn_servers: Vec<SocketAddr>,
}

/// Datagrams handled per readiness wake before the dispatcher yields, so a
/// flood cannot monopolise a front-door worker (the other worker keeps the noq
/// driver and the WebRTC lane running).
const MAX_DATAGRAMS_PER_WAKE: usize = 1024;

impl Dispatcher {
    async fn run(mut self) {
        let mut batch = socket::RecvBatch::new(&self.socket);
        loop {
            if self.socket.readable().await.is_err() {
                Stats::bump(&self.stats.recv_errors);
                tokio::task::yield_now().await;
                continue;
            }
            self.prune_peers();
            // The established socket first: a flood on the listener can never
            // delay a steered peer's datagrams behind its own.
            let mut handled = self.drain(socket::Which::Established, &mut batch, 0);
            handled = self.drain(socket::Which::Listener, &mut batch, handled);
            if handled >= MAX_DATAGRAMS_PER_WAKE {
                tokio::task::yield_now().await;
            }
            self.stats.tracked_sources.store(
                u64::try_from(self.limiter.tracked_sources()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    /// Read `which` until drained or the per-wake budget is spent; returns the
    /// running total.
    fn drain(
        &mut self,
        which: socket::Which,
        batch: &mut socket::RecvBatch,
        mut handled: usize,
    ) -> usize {
        let from_established = which == socket::Which::Established;
        while handled < MAX_DATAGRAMS_PER_WAKE {
            match self.socket.try_recv_batch(which, batch) {
                Ok(0) => break,
                Ok(n) => {
                    let now = Instant::now();
                    for (src, dst_ip, ecn, payload) in batch.datagrams(n) {
                        handled += 1;
                        Stats::bump(if from_established {
                            &self.stats.received_established_socket
                        } else {
                            &self.stats.received_listener_socket
                        });
                        self.dispatch(src, dst_ip, ecn, payload, now, from_established);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    Stats::bump(&self.stats.recv_errors);
                    break;
                }
            }
        }
        handled
    }

    /// Drop the per-peer buckets of peers that left both established sets
    /// (only when a set's membership changed: two atomic loads otherwise).
    fn prune_peers(&mut self) {
        let generation = self
            .quic_established
            .generation()
            .wrapping_add(self.rtc_established.generation());
        if generation == self.peers_generation {
            return;
        }
        self.peers_generation = generation;
        let (quic, rtc, turn) = (
            &self.quic_established,
            &self.rtc_established,
            &self.turn_servers,
        );
        self.peers
            .retain(|a| turn.contains(&a) || quic.contains(a) || rtc.contains(a));
    }

    fn dispatch(
        &mut self,
        src: SocketAddr,
        dst_ip: Option<IpAddr>,
        ecn: Option<noq::udp::EcnCodepoint>,
        payload: &[u8],
        now: Instant,
        from_established_socket: bool,
    ) {
        Stats::bump(&self.stats.received);
        let canonical_src = classify::canonical(src);
        let from_turn = !self.turn_servers.is_empty() && self.turn_servers.contains(&canonical_src);
        let verdict = classify(payload, from_turn);
        let (lane, budget) = match verdict {
            Verdict::Drop(_) => {
                Stats::bump(&self.stats.dropped_classify);
                return;
            }
            Verdict::Rtc if classify::is_stun(payload) => (Lane::Rtc, Budget::Stun),
            Verdict::Rtc => (Lane::Rtc, Budget::None),
            Verdict::Quic(QuicKind::VersionNegotiationTrigger) => {
                (Lane::Quic, Budget::VersionNegotiation)
            }
            Verdict::Quic(QuicKind::Initial) => (Lane::Quic, Budget::Initial),
            Verdict::Quic(_) => (Lane::Quic, Budget::None),
        };
        let (tx, established) = match lane {
            Lane::Quic => (&self.quic, &self.quic_established),
            Lane::Rtc => (&self.rtc, &self.rtc_established),
        };
        // A TURN server we allocated on is validated by configuration. The
        // exact set decides, whichever socket the datagram arrived on: the
        // steering hash may collide, the set cannot.
        let is_established = from_turn || established.contains(src);
        if from_established_socket && !is_established {
            Stats::bump(&self.stats.steered_unverified);
        }
        // Issue #325: a VN-shaped datagram (first byte 128-191, zero version)
        // from an unvalidated source is not RTP anyone sends before ICE.
        if lane == Lane::Rtc && !is_established && classify::is_vn_shaped_rtp(payload) {
            Stats::bump(&self.stats.dropped_classify);
            return;
        }
        let admitted = match budget {
            // A VN trigger always pays its budget: no validated peer needs one.
            Budget::VersionNegotiation => self.limiter.admit(src, budget, now).map(|()| false),
            // Issue #325: the address is spoofable, so an Initial always pays
            // the handshake and global budgets, established source or not.
            Budget::Initial if is_established => {
                self.limiter.admit_initial_established(now).map(|()| false)
            }
            _ if is_established => {
                if !self.peers.admit(canonical_src, now) {
                    Stats::bump(&self.stats.dropped_established_peer);
                    return;
                }
                Ok(true)
            }
            // New-flow limits run before the datagram is copied anywhere.
            _ => self.limiter.admit(src, budget, now).map(|()| false),
        };
        let queue = match admitted {
            Ok(true) => &tx.established,
            Ok(false) => &tx.new,
            Err(refusal) => {
                Stats::bump(match refusal {
                    Refusal::PerSource => &self.stats.dropped_per_source,
                    Refusal::Global => &self.stats.dropped_global,
                    Refusal::VersionNegotiation => &self.stats.dropped_vn,
                    Refusal::Initial => &self.stats.dropped_initial,
                    Refusal::StunPerSource => &self.stats.dropped_stun_per_source,
                    Refusal::StunGlobal => &self.stats.dropped_stun_global,
                });
                return;
            }
        };
        let datagram = Datagram {
            src,
            dst_ip: dst_ip.map(unmap),
            ecn,
            payload: payload.to_vec(),
        };
        match queue.try_send(datagram) {
            Ok(()) => Stats::bump(match lane {
                Lane::Quic => &self.stats.to_quic,
                Lane::Rtc => &self.stats.to_rtc,
            }),
            Err(mpsc::error::TrySendError::Full(_)) => Stats::bump(&self.stats.dropped_queue_full),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Stats::bump(&self.stats.dropped_no_consumer);
            }
        }
    }
}

fn gauge(counter: &AtomicU64, value: usize) {
    counter.store(u64::try_from(value).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// Recompile and attach the steering program when an established set
/// changes. Off the data plane: it runs on the front-door runtime, debounces,
/// snapshots both sets (bounded by their caps), compiles one program (bounded
/// by [`STEERING_MAX_PEERS`]) and attaches it with one `setsockopt`; the
/// kernel swaps it atomically. A failed attach keeps the previous program.
async fn steering_updater(
    socket: Arc<SharedSocket>,
    stats: Arc<Stats>,
    quic: Arc<EstablishedSet>,
    rtc: Arc<EstablishedSet>,
    turn_servers: Vec<SocketAddr>,
) {
    gauge(&stats.steered_peers, turn_servers.len());
    let mut last_warn: Option<Instant> = None;
    loop {
        tokio::select! {
            () = quic.changed() => {}
            () = rtc.changed() => {}
        }
        tokio::time::sleep(STEERING_DEBOUNCE).await;
        let peers = quic
            .snapshot()
            .into_iter()
            .chain(rtc.snapshot())
            .chain(turn_servers.iter().copied());
        let set = ProgramSet::build(steering::random_salt(), peers);
        match socket.attach(&set) {
            Ok(()) => {
                gauge(&stats.steered_peers, set.steered());
                gauge(&stats.steering_overflow, set.overflow());
                Stats::bump(&stats.steering_updates);
            }
            Err(e) => {
                Stats::bump(&stats.steering_update_errors);
                if last_warn.is_none_or(|t| t.elapsed() >= WARN_INTERVAL) {
                    last_warn = Some(Instant::now());
                    tracing::warn!(
                        error = %e,
                        peers = set.steered(),
                        "front door: steering update failed; the previous program stays"
                    );
                }
            }
        }
    }
}

/// Sample the kernel receive drops of both sockets every
/// [`KERNEL_DROP_SAMPLE`] into the stats gauges, and warn (at most once per
/// 10 s) when they grow.
async fn kernel_drop_sampler(socket: Arc<SharedSocket>, stats: Arc<Stats>) {
    let (listener, established) = socket.inodes();
    let mut tick = tokio::time::interval(KERNEL_DROP_SAMPLE);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = (0u64, 0u64);
    let mut last_warn: Option<Instant> = None;
    loop {
        tick.tick().await;
        // procfs read on the blocking pool, never on a worker.
        let Ok(Ok(table)) =
            tokio::task::spawn_blocking(|| std::fs::read_to_string("/proc/net/udp6")).await
        else {
            continue;
        };
        let l = listener
            .and_then(|i| parse_udp_drops(&table, i))
            .unwrap_or(0);
        let e = established
            .and_then(|i| parse_udp_drops(&table, i))
            .unwrap_or(0);
        stats.kernel_drops_listener.store(l, Ordering::Relaxed);
        stats.kernel_drops_established.store(e, Ordering::Relaxed);
        if (l > last.0 || e > last.1) && last_warn.is_none_or(|t| t.elapsed() >= WARN_INTERVAL) {
            last_warn = Some(Instant::now());
            tracing::warn!(
                listener_drops = l,
                established_drops = e,
                "front door: the kernel dropped UDP datagrams (receive queue full); see \
                 docs/runbooks/udp-443-flood.md"
            );
        }
        last = (l, e);
    }
}

/// IPv4-mapped IPv6 to IPv4.
fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}
