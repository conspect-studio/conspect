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
//! headers (Version Negotiation triggers) also pay a global VN budget. The
//! established set is written only by the consumers after validation and is
//! capped by the session/connection caps. Nothing grows with attacker traffic.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod classify;
pub mod limits;
pub mod quic;
pub mod rtc;
mod socket;

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

pub use classify::{classify, first_byte_class, ByteClass, DropReason, Lane, QuicKind, Verdict};
pub use limits::{Budget, EstablishedSet, NewFlowLimiter, NewFlowLimits, Refusal};
pub use quic::QuicLane;
pub use rtc::RtcLane;
pub use socket::SharedSocket;

/// Established-queue depth per lane (design note section 13).
pub const ESTABLISHED_QUEUE: usize = 1024;
/// New-flow-queue depth per lane (design note section 13).
pub const NEW_QUEUE: usize = 256;
/// How long [`FrontDoor::shutdown_blocking`] waits for the runtime's tasks.
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Default cap on established QUIC peers (`network.quic.max_connections`).
pub const DEFAULT_MAX_QUIC_PEERS: usize = 1024;

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
    /// addresses may be TURN `ChannelData` (first byte 64-79).
    pub turn_servers: Vec<SocketAddr>,
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
        let socket = {
            // The tokio socket registers with the reactor of the runtime it is
            // created in: the front door's, never the caller's.
            let _guard = runtime.enter();
            SharedSocket::bind(addr, config.recv_buffer_bytes)
                .map_err(|source| Error::Socket { addr, source })?
        };
        let socket = Arc::new(socket);
        let stats = Arc::new(Stats::default());
        let quic_established = Arc::new(EstablishedSet::new(config.max_quic_peers));
        let rtc_established = Arc::new(EstablishedSet::new(config.max_rtc_peers));
        let (quic_tx, quic_rx) = lane_channel();
        let (rtc_tx, rtc_rx) = lane_channel();
        let dispatcher = Dispatcher {
            socket: Arc::clone(&socket),
            stats: Arc::clone(&stats),
            limiter: NewFlowLimiter::new(config.limits, Instant::now()),
            quic: quic_tx,
            rtc: rtc_tx,
            quic_established: Arc::clone(&quic_established),
            rtc_established: Arc::clone(&rtc_established),
            turn_servers: config
                .turn_servers
                .iter()
                .copied()
                .map(classify::canonical)
                .collect(),
        };
        runtime.spawn(dispatcher.run());
        tracing::info!(
            local = %socket.local_addr(),
            workers = config.workers.max(1),
            rcvbuf = socket.recv_buffer_size().unwrap_or(0),
            "front door: one UDP socket for QUIC and WebRTC on its own runtime"
        );
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

/// The one task that reads the socket and feeds the lanes.
struct Dispatcher {
    socket: Arc<SharedSocket>,
    stats: Arc<Stats>,
    limiter: NewFlowLimiter,
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
            let mut handled = 0usize;
            while handled < MAX_DATAGRAMS_PER_WAKE {
                match self.socket.try_recv_batch(&mut batch) {
                    Ok(0) => break,
                    Ok(n) => {
                        let now = Instant::now();
                        for (src, dst_ip, ecn, payload) in batch.datagrams(n) {
                            handled += 1;
                            self.dispatch(src, dst_ip, ecn, payload, now);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        Stats::bump(&self.stats.recv_errors);
                        break;
                    }
                }
            }
            if handled >= MAX_DATAGRAMS_PER_WAKE {
                tokio::task::yield_now().await;
            }
            self.stats.tracked_sources.store(
                u64::try_from(self.limiter.tracked_sources()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    fn dispatch(
        &mut self,
        src: SocketAddr,
        dst_ip: Option<IpAddr>,
        ecn: Option<noq::udp::EcnCodepoint>,
        payload: &[u8],
        now: Instant,
    ) {
        Stats::bump(&self.stats.received);
        let from_turn =
            !self.turn_servers.is_empty() && self.turn_servers.contains(&classify::canonical(src));
        let verdict = classify(payload, from_turn);
        let (lane, budget) = match verdict {
            Verdict::Drop(_) => {
                Stats::bump(&self.stats.dropped_classify);
                return;
            }
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
        // A TURN server we allocated on is validated by configuration.
        // A VN trigger always pays its budget: no validated peer needs one.
        let is_established =
            budget != Budget::VersionNegotiation && (from_turn || established.contains(src));
        let queue = if is_established {
            &tx.established
        } else {
            // New-flow limits run before the datagram is copied anywhere.
            if let Err(refusal) = self.limiter.admit(src, budget, now) {
                Stats::bump(match refusal {
                    Refusal::PerSource => &self.stats.dropped_per_source,
                    Refusal::Global => &self.stats.dropped_global,
                    Refusal::VersionNegotiation => &self.stats.dropped_vn,
                    Refusal::Initial => &self.stats.dropped_initial,
                });
                return;
            }
            &tx.new
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

/// IPv4-mapped IPv6 to IPv4.
fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}
