//! ST 2110 UDP receive transport — **feature `st2110`**.
//!
//! This module owns the *testable core* of an ST 2110 receive source: the
//! application-layer [`PacketSource`] seam that delivers post-receive RTP packet
//! units, the ST 2022-7 sequence-keyed dedup over that seam
//! ([`DualPathPacketSource`]), and the
//! [`St2110Producer`]: a [`FrameProducer`](crate::source::FrameProducer) that
//! drives the IN-1 [`FrameAssembler`](crate::st2110::assembler::FrameAssembler)
//! and rebases the RTP 90 kHz timestamps onto the nanosecond timeline (via the
//! [`PtsNormalizer`](crate::normalize) the [`IngestPump`] holds).
//!
//! ## The seam vs. the NIC
//!
//! The pure depacketizers in the parent module ([`super::rtp`] / [`super::v20`]
//! …) turn bytes into typed values; the [`St2110Producer`] turns a stream of
//! those into produced frames. Both are driven by **injected** packet units and
//! are fully unit-tested with no NIC. The actual UDP sockets — [`RtpReceiver`]
//! and the two-path [`DualPathReceiver`] — bind a `tokio` `UdpSocket`, join a
//! multicast group, and need a real ST 2110 network (a PTP-disciplined NIC, an
//! IGMP-joined multicast source); this devcontainer has none, so that socket
//! path is exercised only by the gated/`#[ignore]`d live test.
//! [`channel_bridge`](RtpReceiver::channel_bridge) connects the async socket to
//! the sync seam through a **bounded, drop-oldest** channel so a stalled reader
//! never back-pressures the receive task (invariant #10).
//!
//! ## PTP / ST 2059 (informs, never paces)
//!
//! Per [streaming-gotchas §5] the master clock stays `CLOCK_MONOTONIC`; the
//! output is **never** slaved to an input. The RTP 90 kHz media clock is used
//! for per-input PTS only, and the first frame anchors to the master clock's
//! "now" exactly like every other source (the [`IngestPump`] does this). A PTP
//! epoch may later *inform* the rebase anchor, but full ST 2059 lock is out of
//! scope here (the brief's "free-run the rest" guidance).
//!
//! ## No native FFI (pure / LGPL-clean default)
//!
//! It deliberately does **no** native FFI (uses `tokio`'s safe `UdpSocket`),
//! keeping `multiview-input` `unsafe_code = forbid`, and it never paces the output
//! clock: received datagrams are depacketized and handed to the ingest pipeline
//! (which writes the last-good-frame store), exactly like every other source
//! (invariants #1 / #10).
//!
//! [`IngestPump`]: crate::source::IngestPump
//! [streaming-gotchas §5]: ../../../docs/research/streaming-gotchas.md

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;

use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};

use crate::error::{Error, Result};
use crate::normalize::WrapBits;
use crate::source::{FrameProducer, ProducedFrame};
use crate::st2022_7::{HitlessReconstructor, Path, PushOutcome};
use crate::st2110::assembler::{FrameAssembler, PacketUnit, RasterGeometry};
use crate::st2110::rtp::RtpPacket;
use crate::st2110::v20::V20Payload;

/// The RTP media clock rate ST 2110-20 video rides on (90 kHz, RFC 4175).
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// The largest UDP datagram an ST 2110 receiver will buffer. ST 2110-20 uses a
/// ~1500-byte standard MTU (or up to ~9000 jumbo); 9 KiB covers both with margin.
pub const MAX_DATAGRAM: usize = 9216;

/// Which network interface to join an IPv6 multicast group on.
///
/// IPv6 multicast is **interface-scoped** (RFC 4291 scope-id): the OS index `0`
/// ("unspecified" — let the kernel pick) is not portable and commonly fails for
/// **link-local** (`ff02::/16`) and **site-local** (`ff05::/16`) groups, which
/// must name a concrete interface. A deployment supplies the intended interface
/// (from config or the SDP), so the RX wiring is not silently pinned to index 0
/// (panel F6). Applies to the IPv6 path only — IPv4 multicast joins via
/// `INADDR_ANY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MulticastInterface {
    /// Let the OS pick the default multicast interface (IPv6 index `0`).
    /// Portable for a global-scope group on a single-homed host; NOT reliable
    /// for link/site-local groups or a multi-homed host.
    #[default]
    Unspecified,
    /// A specific IPv6 interface by its OS index / scope-id (e.g. resolved from
    /// an interface name or supplied by config). Required for link/site-local
    /// groups and to pin the join on a multi-homed host.
    Index(u32),
}

impl MulticastInterface {
    /// The IPv6 interface index this selection passes to
    /// [`join_multicast_v6`](RtpReceiver::join_multicast_v6): `0` for
    /// [`Unspecified`](Self::Unspecified), else the explicit index.
    #[must_use]
    pub const fn index(self) -> u32 {
        match self {
            MulticastInterface::Unspecified => 0,
            MulticastInterface::Index(index) => index,
        }
    }
}

/// A single-path ST 2110 RTP receive socket.
///
/// Binds a UDP socket and (optionally) joins a source-specific multicast group,
/// then yields raw RTP packets one datagram at a time. Parsing is delegated to
/// the pure [`RtpPacket::parse`]; this type owns only the socket and a receive
/// buffer.
#[derive(Debug)]
pub struct RtpReceiver {
    socket: UdpSocket,
    buf: Vec<u8>,
}

impl RtpReceiver {
    /// Bind a receive socket to `local`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Ingest(format!("st2110 bind {local}: {e}")))?;
        Ok(Self {
            socket,
            buf: vec![0u8; MAX_DATAGRAM],
        })
    }

    /// Join an IPv4 multicast `group` on the interface address `interface`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the membership cannot be joined.
    pub fn join_multicast_v4(&self, group: Ipv4Addr, interface: Ipv4Addr) -> Result<()> {
        self.socket
            .join_multicast_v4(group, interface)
            .map_err(|e| Error::Ingest(format!("st2110 join {group}: {e}")))
    }

    /// Join an IPv6 multicast `group` on the interface with OS index `interface`
    /// (`0` = the default multicast interface). The IPv6-first counterpart of
    /// [`join_multicast_v4`](Self::join_multicast_v4) (ADR-0042).
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the membership cannot be joined.
    pub fn join_multicast_v6(&self, group: Ipv6Addr, interface: u32) -> Result<()> {
        self.socket
            .join_multicast_v6(&group, interface)
            .map_err(|e| Error::Ingest(format!("st2110 join {group}: {e}")))
    }

    /// Join a multicast `group` of either family, dispatching on the address
    /// family so the pipeline wiring makes one family-agnostic
    /// `rx.join_multicast(group, interface)` call regardless of an SDP
    /// `c=IN IP4` / `c=IN IP6` line (ADR-0042 IPv6-first).
    ///
    /// `interface` selects the IPv6 multicast interface (its scope-id / OS index)
    /// — required for link/site-local IPv6 groups, where the OS default
    /// ([`MulticastInterface::Unspecified`] → index `0`) is not portable
    /// (panel F6). The IPv4 path always joins via `INADDR_ANY`; the interface
    /// selection does not apply to it. **Live validation on a real IPv6 multicast
    /// network is hardware-gated** (rule 26) — the interface-index selection is
    /// plumbed here and exercised by the join tests on the devcontainer's
    /// multicast-capable interface.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the membership cannot be joined (e.g. the group's
    /// family does not match the bound socket's family, or the interface index
    /// names no interface).
    pub fn join_multicast(&self, group: IpAddr, interface: MulticastInterface) -> Result<()> {
        match group {
            IpAddr::V4(v4) => self.join_multicast_v4(v4, Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(v6) => self.join_multicast_v6(v6, interface.index()),
        }
    }

    /// Receive one datagram and return the number of payload bytes read into the
    /// internal buffer; the bytes are then available via [`RtpReceiver::buffer`].
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the receive fails.
    pub async fn recv(&mut self) -> Result<usize> {
        let n = self
            .socket
            .recv(&mut self.buf)
            .await
            .map_err(|e| Error::Ingest(format!("st2110 recv: {e}")))?;
        Ok(n)
    }

    /// The first `len` bytes of the last received datagram.
    #[must_use]
    pub fn buffer(&self, len: usize) -> &[u8] {
        let end = len.min(self.buf.len());
        self.buf.get(..end).unwrap_or(&[])
    }

    /// Receive one datagram and parse it as an RTP packet, returning the parsed
    /// view borrowed from the internal buffer.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] on a socket error or an RTP parse failure.
    pub async fn recv_rtp(&mut self) -> Result<RtpPacket<'_>> {
        let n = self.recv().await?;
        let end = n.min(self.buf.len());
        let bytes = self.buf.get(..end).unwrap_or(&[]);
        RtpPacket::parse(bytes).map_err(|e| Error::Ingest(format!("st2110 rtp: {e}")))
    }

    /// Drive this socket into a bounded, drop-oldest [`ChannelPacketSource`].
    ///
    /// Returns the sync [`PacketSource`] half plus an async future that loops
    /// `recv_rtp`, converts each datagram into an [`St2110Packet`], and enqueues
    /// it. When the channel is full the **oldest** queued unit is dropped so the
    /// receive loop never blocks on a slow reader (invariant #10): an ST 2110
    /// receiver must never let downstream stall back-pressure the wire. The
    /// future ends (clean end-of-stream) if the receiver half is dropped or a
    /// socket error occurs; it is the live, NIC-bound path and is exercised only
    /// by the gated live test.
    #[must_use]
    pub fn channel_bridge(mut self, capacity: usize) -> (ChannelPacketSource, ReceiveLoop) {
        let (sink, source) = ChannelPacketSource::bounded(capacity);
        let task = async move {
            loop {
                // Stop as soon as the sync reader is gone — nothing to feed.
                if sink.is_closed() {
                    return;
                }
                let unit = match self.recv_rtp().await {
                    Ok(packet) => St2110Packet::from_rtp(&packet),
                    // A socket/parse error ends the loop; the supervisor
                    // reconnects. Dropping the sink signals clean EOS to the
                    // sync side.
                    Err(_) => return,
                };
                // Genuine drop-oldest on a full ring (ADR-0033 §7): the push
                // never blocks and never grows — the freshest media is retained
                // and a stalled reader can never back-pressure the wire
                // (invariant #10).
                sink.push(unit);
            }
        };
        (source, Box::pin(task))
    }
}

/// The async receive future returned by [`RtpReceiver::channel_bridge`]: it must
/// be polled (e.g. `tokio::spawn`ed) to drive the live socket into the bounded
/// channel. It never resolves until the socket faults or the reader is dropped.
pub type ReceiveLoop = core::pin::Pin<Box<dyn core::future::Future<Output = ()> + Send>>;

/// A two-path ST 2022-7 receiver: two [`RtpReceiver`]s feeding one
/// [`HitlessReconstructor`] keyed by RTP sequence number.
///
/// `P` is the per-packet payload the caller extracts from each datagram (e.g. an
/// owned copy of the RTP payload). The reconstructor lives in the always-compiled
/// pure module; this struct is only the socket wiring around it.
#[derive(Debug)]
pub struct DualPathReceiver<P> {
    path_a: RtpReceiver,
    path_b: RtpReceiver,
    reconstructor: HitlessReconstructor<P>,
}

impl<P> DualPathReceiver<P> {
    /// Build a dual-path receiver from two bound sockets and a reorder-window
    /// capacity.
    #[must_use]
    pub fn new(path_a: RtpReceiver, path_b: RtpReceiver, window: usize) -> Self {
        Self {
            path_a,
            path_b,
            reconstructor: HitlessReconstructor::new(window),
        }
    }

    /// Receive concurrently from both paths; whichever delivers first is parsed,
    /// its payload extracted by `extract`, pushed into the reconstructor, and the
    /// merged in-order packets returned.
    ///
    /// `extract` maps a parsed [`RtpPacket`] to the caller's payload `P` (e.g. an
    /// owned payload copy). The merge/de-dup/reorder is performed by the pure
    /// reconstructor.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] on a socket error or RTP parse failure on the path that
    /// fired.
    pub async fn recv_merged<F>(&mut self, mut extract: F) -> Result<Vec<P>>
    where
        F: FnMut(&RtpPacket<'_>) -> P,
    {
        let (path, packet) = tokio::select! {
            r = self.path_a.recv_rtp() => (Path::A, r?),
            r = self.path_b.recv_rtp() => (Path::B, r?),
        };
        let payload = extract(&packet);
        let _ = self
            .reconstructor
            .push(path, packet.header.sequence, payload);
        Ok(self.reconstructor.drain())
    }

    /// Borrow the underlying reconstructor (for metrics / window inspection).
    #[must_use]
    pub const fn reconstructor(&self) -> &HitlessReconstructor<P> {
        &self.reconstructor
    }
}

/// One post-receive ST 2110-20 RTP packet unit handed across the
/// [`PacketSource`] seam.
///
/// This is the boundary between the (NIC-bound, gated) socket layer and the
/// pure depacketize -> assemble -> frame logic: the socket parses the RTP fixed
/// header and hands these fields here; the producer never sees the wire framing
/// again, only this typed unit. `payload` is an **owned** copy of the RTP
/// payload (the bytes after the fixed header) the SRD segments point into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct St2110Packet {
    /// The RFC 4175 marker bit: `true` flags the **last packet of a frame**.
    pub marker: bool,
    /// The 32-bit RTP media timestamp. Units are essence-specific: 90 kHz for
    /// ST 2110-20 video (one value per frame), the audio sample rate for
    /// ST 2110-30 / AES67 (e.g. 48 kHz, advancing by sample-groups per packet).
    pub timestamp: u32,
    /// The 16-bit RTP sequence number (gap detection / 2022-7 dedup / reorder).
    pub sequence: u16,
    /// The 32-bit RTP synchronization source. The video assembler ignores it;
    /// the ST 2110-30 / AES67 audio path forwards it to the
    /// [`RtpAudioRebaser`](crate::rtp_audio) so a genuine SSRC change re-anchors.
    pub ssrc: u32,
    /// The 7-bit RTP payload type (RFC 3550 §5.1). The video assembler ignores it
    /// (like `ssrc`); the ST 2110-30 / AES67 audio producer filters on it so a
    /// stray / multiplexed RTP stream sharing the 5-tuple is not decoded as PCM.
    pub payload_type: u8,
    /// The owned RTP payload bytes (after the fixed header) the SRD segments of
    /// the depacketized [`V20Payload`] index into.
    pub payload: Vec<u8>,
}

impl St2110Packet {
    /// Build a packet unit from a parsed [`RtpPacket`], copying its payload.
    ///
    /// This is how the socket layer crosses the seam: it owns the borrowed
    /// payload so the unit can outlive the receive buffer.
    #[must_use]
    pub fn from_rtp(packet: &RtpPacket<'_>) -> Self {
        Self {
            marker: packet.header.marker,
            timestamp: packet.header.timestamp,
            sequence: packet.header.sequence,
            ssrc: packet.header.ssrc,
            payload_type: packet.header.payload_type,
            payload: packet.payload.to_vec(),
        }
    }
}

/// The application-layer packet-source seam.
///
/// A concrete implementation delivers post-receive ST 2110-20 RTP packet units.
/// It is driven cooperatively by [`St2110Producer`]: each
/// [`poll_packet`](PacketSource::poll_packet) returns the next available unit,
/// `Ok(None)` at clean end-of-stream (or when nothing is currently ready), or an
/// error the supervisor reacts to. It must **never block the caller** waiting on
/// the network — a source with nothing ready returns `Ok(None)` and is held
/// (invariants #1 / #10).
pub trait PacketSource {
    /// Pull the next post-receive packet unit.
    ///
    /// Returns `Ok(Some(unit))` for a unit, `Ok(None)` at clean end-of-stream
    /// (or when nothing is currently ready), or an error for a fault the
    /// supervisor should react to (reconnect).
    ///
    /// # Errors
    ///
    /// An [`Error`] when the underlying source faults (a socket error, an RTP
    /// parse failure). The caller treats this as a connection fault and applies
    /// the supervised-reconnect backoff rather than crashing the engine.
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>>;
}

/// A two-path ST 2022-7 dedup over the [`PacketSource`] seam.
///
/// Wraps two [`PacketSource`]s (the redundant network paths) and one
/// [`HitlessReconstructor`] keyed by RTP sequence number: each
/// [`poll_packet`](PacketSource::poll_packet) drains both paths into the
/// reconstructor, which **de-duplicates** the copy of every sequence that
/// arrives on both paths and releases the merged units in sequence order. A
/// packet lost on one path but present on the other produces **no gap**; a
/// packet lost on *both* is a genuine gap the assembler surfaces downstream.
///
/// This is the pure, NIC-free analogue of [`DualPathReceiver::recv_merged`]: the
/// merge/dedup logic is identical (the same reconstructor), but driven by the
/// injectable sync seam so it is fully unit-tested without a socket.
pub struct DualPathPacketSource {
    path_a: Box<dyn PacketSource + Send>,
    path_b: Box<dyn PacketSource + Send>,
    reconstructor: HitlessReconstructor<St2110Packet>,
    /// Units released by the reconstructor but not yet returned (FIFO), so each
    /// `poll_packet` returns exactly one merged unit.
    pending: std::collections::VecDeque<St2110Packet>,
}

impl DualPathPacketSource {
    /// Build a dual-path dedup over two packet sources and a reorder-window
    /// `capacity` (distinct sequence numbers held before the window slides).
    #[must_use]
    pub fn new(
        path_a: Box<dyn PacketSource + Send>,
        path_b: Box<dyn PacketSource + Send>,
        capacity: usize,
    ) -> Self {
        Self {
            path_a,
            path_b,
            reconstructor: HitlessReconstructor::new(capacity),
            pending: std::collections::VecDeque::new(),
        }
    }

    /// Push one unit from `path` into the reconstructor (de-dup by sequence),
    /// then queue any newly-released merged units.
    fn ingest(&mut self, path: Path, unit: St2110Packet) {
        let seq = unit.sequence;
        // The reconstructor owns the dedup: a second copy of `seq` from the other
        // path returns `Duplicate` and is dropped here.
        if self.reconstructor.push(path, seq, unit) == PushOutcome::Accepted {
            for released in self.reconstructor.drain() {
                self.pending.push_back(released);
            }
        }
    }
}

impl PacketSource for DualPathPacketSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>> {
        // Drain both paths into the reconstructor until a merged unit is ready or
        // neither path has anything to deliver. Each pull is non-blocking; a
        // `None` from a path means "nothing ready this tick" (it is NOT latched
        // as permanent end-of-stream — a live channel source returns `None`
        // whenever its buffer is momentarily empty).
        loop {
            if let Some(unit) = self.pending.pop_front() {
                return Ok(Some(unit));
            }
            let mut progressed = false;
            if let Some(unit) = self.path_a.poll_packet()? {
                self.ingest(Path::A, unit);
                progressed = true;
            }
            if let Some(unit) = self.path_b.poll_packet()? {
                self.ingest(Path::B, unit);
                progressed = true;
            }
            if !progressed {
                // A lull: neither path delivered. Release everything the reorder
                // window is still holding back — the network has caught up, so
                // there is nothing newer to reorder against, and a frame's final
                // packet must not be stranded waiting (invariant #1). This is
                // correct whether the lull is transient (a live channel) or the
                // genuine end of an injected stream.
                for released in self.reconstructor.flush() {
                    self.pending.push_back(released);
                }
                return Ok(self.pending.pop_front());
            }
        }
    }
}

impl core::fmt::Debug for DualPathPacketSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DualPathPacketSource")
            .field("buffered", &self.reconstructor.buffered())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

/// Shared bounded-ring state behind the [`PacketSink`] producer half and the
/// [`ChannelPacketSource`] consumer half.
///
/// A plain [`VecDeque`](std::collections::VecDeque) guarded by a `Mutex` held
/// only for the O(1) push/pop — never across a socket call or an `.await` — so
/// the *property* it provides is genuine bounded **drop-oldest**: a full ring
/// sheds its oldest unit instead of growing, and neither half ever blocks the
/// other (invariants #1 / #5 / #10). Mirrors the display-audio sink's
/// `AudioFifo`.
#[derive(Debug)]
struct PacketRing {
    queue: std::collections::VecDeque<St2110Packet>,
    capacity: usize,
    dropped: u64,
    /// Set when the consuming [`ChannelPacketSource`] is dropped, so the
    /// producing receive task learns the reader is gone and can stop.
    consumer_gone: bool,
}

/// The producer half of the receive bridge, handed to the async receive task.
///
/// [`push`](PacketSink::push) is a bounded, genuine **drop-oldest** enqueue:
/// on a full ring the *oldest* queued unit is evicted so the freshest media is
/// retained (ADR-0033 §7). It never blocks and never grows, so a stalled reader
/// can never back-pressure the receive task or the wire (invariant #10).
/// Cloneable so the receive task can hold it while the sync source drains the
/// other half.
#[derive(Debug, Clone)]
pub struct PacketSink {
    ring: Arc<Mutex<PacketRing>>,
}

impl PacketSink {
    /// Enqueue one unit, evicting the **oldest** queued unit first when the ring
    /// is already at capacity (genuine drop-oldest — ADR-0033 §7). Never blocks;
    /// the receive task is never back-pressured by a slow reader (invariant #10).
    pub fn push(&self, unit: St2110Packet) {
        // `try_lock`, never a blocking `lock`: the tokio receive task must never
        // wait on the sync consumer holding the ring (inv #10 — a stalled reader
        // can never back-pressure the wire). On contention (or a poisoned lock —
        // a holder panicked) the unit is shed; the critical section is a few O(1)
        // `VecDeque` ops so genuine contention is nanosecond-scale, and bounded /
        // never-grow is preserved (only the freshest unit is ever at risk).
        let Ok(mut ring) = self.ring.try_lock() else {
            return;
        };
        // Genuine drop-oldest (ADR-0033 §7): a full ring evicts its OLDEST unit
        // before appending the newest, so the freshest media is retained and the
        // ring never grows past capacity.
        while ring.queue.len() >= ring.capacity {
            ring.queue.pop_front();
            ring.dropped = ring.dropped.saturating_add(1);
        }
        ring.queue.push_back(unit);
    }

    /// Whether the consuming [`ChannelPacketSource`] has been dropped (the reader
    /// is gone, so the receive task should stop).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        // `try_lock`, never block the receive task on the consumer's lock
        // (inv #10). Momentary contention is "not closed" (re-checked next loop
        // iteration); a poisoned lock (a holder panicked) is treated as closed so
        // the task stops rather than spins.
        match self.ring.try_lock() {
            Ok(ring) => ring.consumer_gone,
            Err(std::sync::TryLockError::WouldBlock) => false,
            Err(std::sync::TryLockError::Poisoned(_)) => true,
        }
    }
}

/// A bounded, drop-oldest packet source fed by an async receive task.
///
/// This is the seam the live (NIC-bound) [`RtpReceiver`] / [`DualPathReceiver`]
/// path crosses into the sync [`St2110Producer`]: the async receive loop pushes
/// units into a **bounded ring** via a [`PacketSink`]; this source drains the
/// front. A stalled reader can never back-pressure the sender — the sink drops
/// the *oldest* queued unit when the ring is full (invariant #10). It never
/// blocks the data plane: an empty ring yields `Ok(None)` (the producer re-polls
/// next tick), and a dropped sink (the receive task ended) simply drains to
/// empty and then yields `Ok(None)` (clean end-of-stream).
#[derive(Debug)]
pub struct ChannelPacketSource {
    ring: Arc<Mutex<PacketRing>>,
}

impl ChannelPacketSource {
    /// Build a bounded ring of `capacity` units, returning the [`PacketSink`]
    /// producer half (for the async receive task) and the sync [`PacketSource`]
    /// consumer half.
    ///
    /// The receive task calls [`PacketSink::push`], which on a full ring drops
    /// the **oldest** unit so a slow reader never stalls the receiver — the ring
    /// is bounded and never grows (invariant #10 / #5).
    #[must_use]
    pub fn bounded(capacity: usize) -> (PacketSink, Self) {
        let ring = Arc::new(Mutex::new(PacketRing {
            queue: std::collections::VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
            consumer_gone: false,
        }));
        (
            PacketSink {
                ring: Arc::clone(&ring),
            },
            Self { ring },
        )
    }

    /// The number of units dropped to drop-oldest overflow since construction
    /// (telemetry / test observability).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        // Telemetry read: `try_lock`, never block the data plane; report 0 on the
        // rare momentary contention (the caller re-reads next time).
        self.ring.try_lock().map_or(0, |ring| ring.dropped)
    }
}

impl Drop for ChannelPacketSource {
    fn drop(&mut self) {
        // Source teardown — NOT the engine data plane. This Drop runs when the
        // ingest consumer (the sync `St2110Producer`) is torn down, off the output
        // clock, so inv #10 (a client/consumer must never back-pressure the ENGINE)
        // does not bind here: there is no engine tick waiting on this lock.
        //
        // A blocking `lock` is the RIGHT choice, and its block is bounded: the ring
        // `Mutex` is only ever held for O(1) `VecDeque` push/pop/flag ops and NEVER
        // across an `.await` or a socket `send`/`recv` (see `PacketSink::push` and
        // `poll_packet`, which take it only for those O(1) sections). The single
        // other contender is the receive task's `push`, so the worst-case wait is
        // ≤ one push's critical section — a few nanoseconds, never an unbounded
        // stall — and it cannot deadlock (the consumer's `poll_packet` has already
        // released; nothing holds this lock across a suspension point).
        //
        // Why NOT `try_lock` here (unlike `push`/`poll_packet`/`dropped`, which shed
        // on contention): Drop runs EXACTLY ONCE, and setting `consumer_gone` is the
        // only signal the producing receive task gets that the reader is gone. A
        // `try_lock` that lost the nanosecond race would drop the flag forever (Drop
        // never runs again) and leave that task spinning on a dead stream until its
        // socket faults. A guaranteed, bounded one-time block is strictly better
        // than a best-effort teardown signal. The inv #10 non-blocking guarantee on
        // the live path stays on `push`/`poll_packet`, which `try_lock`.
        if let Ok(mut ring) = self.ring.lock() {
            ring.consumer_gone = true;
        }
    }
}

impl PacketSource for ChannelPacketSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>> {
        // `try_lock`, never a blocking `lock`: the ingest consumer must never
        // wait on the tokio receive task holding the ring (inv #10 / F2). Pop the
        // oldest buffered unit. An empty ring (nothing ready this tick — hold,
        // never block, inv #1), momentary lock contention (or a poisoned lock),
        // and a dropped sink (the receive task ended — clean end-of-stream) all
        // surface to the non-blocking producer as "no frame now"; the producer
        // re-polls next tick.
        Ok(self
            .ring
            .try_lock()
            .ok()
            .and_then(|mut ring| ring.queue.pop_front()))
    }
}

/// A [`FrameProducer`] over a [`PacketSource`]: pulls ST 2110-20 RTP packet
/// units, depacketizes each ([`V20Payload::parse`]) and feeds the IN-1
/// [`FrameAssembler`], and yields a produced frame per reassembled raster for
/// the [`IngestPump`].
///
/// This is the IN-2 bridge for ST 2110: it does **non-blocking pulls only** from
/// the source and never paces the output clock. The assembler's bounded raster
/// buffers drop, never grow (invariants #1 / #2 / #5). The RTP 90 kHz timestamp
/// is surfaced as the producer's raw PTS; [`St2110Producer::wrap_bits`] reports
/// [`WrapBits::Rtp32`] so the [`PtsNormalizer`](crate::normalize) rebases it onto
/// the nanosecond timeline correctly (the float-free 90 kHz -> ns conversion).
///
/// [`IngestPump`]: crate::source::IngestPump
pub struct St2110Producer {
    source: Box<dyn PacketSource + Send>,
    assembler: FrameAssembler,
}

impl St2110Producer {
    /// The maximum number of source packets a single [`next_frame`](FrameProducer::next_frame)
    /// call pulls before yielding `Ok(None)`.
    ///
    /// A sample must be **bounded work**: a malformed-packet flood must never
    /// make one poll loop until the source drains, which would delay the output
    /// clock (invariant #1). After this many pulled units without a closed
    /// frame the call returns `Ok(None)` and the pump re-polls next tick, so a
    /// flood is drained at up to this rate per tick rather than all at once.
    /// Sized well above any real per-tick packet count (a raster is a handful of
    /// SRD units per frame, one frame per tick).
    pub const MAX_PACKETS_PER_POLL: usize = 64;

    /// Build a producer over an application-supplied [`PacketSource`],
    /// reassembling into `geometry`.
    #[must_use]
    pub fn new(source: Box<dyn PacketSource + Send>, geometry: RasterGeometry) -> Self {
        Self {
            source,
            assembler: FrameAssembler::new(geometry),
        }
    }

    /// The raster geometry this producer reassembles into.
    #[must_use]
    pub const fn geometry(&self) -> RasterGeometry {
        self.assembler.geometry()
    }

    /// Depacketize one packet unit and push it into the assembler, returning a
    /// closed frame mapped to a [`ProducedFrame`] when one becomes available.
    ///
    /// A malformed -20 payload is dropped (it never closes a frame and never
    /// panics) rather than faulting the source — a single bad datagram must not
    /// stall the stream (invariants #1 / #2).
    fn push(&mut self, packet: &St2110Packet) -> Option<ProducedFrame> {
        let payload_v20 = V20Payload::parse(&packet.payload, packet.sequence).ok()?;
        let unit = PacketUnit {
            marker: packet.marker,
            timestamp: packet.timestamp,
            sequence: packet.sequence,
            payload: packet.payload.clone(),
            payload_v20,
        };
        let assembled = self.assembler.push(&unit)?;
        let geometry = self.assembler.geometry();
        Some(ProducedFrame {
            pixels: assembled.pixels,
            // The verbatim 90 kHz RTP timestamp; the pump rebases it via the
            // normalizer ([`WrapBits::Rtp32`]) onto the ns timeline.
            raw_pts: Some(assembled.raw_pts),
            // A sequence gap / lost-marker partial re-anchors the normalizer.
            discontinuity: assembled.discontinuity,
            meta: FrameMeta {
                pts: MediaTime::ZERO,
                width: geometry.width(),
                height: geometry.height(),
                format: PixelFormat::Nv12,
                color: ColorInfo::default(),
            },
        })
    }
}

impl core::fmt::Debug for St2110Producer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("St2110Producer")
            .field("geometry", &self.assembler.geometry())
            .finish_non_exhaustive()
    }
}

impl FrameProducer for St2110Producer {
    /// Pull source packets until a complete raster emerges, the source signals
    /// clean end-of-stream, or the per-poll budget is spent. Each pull is
    /// non-blocking; an empty pull ends this call (the pump re-polls on the next
    /// tick) rather than spinning.
    ///
    /// **Bounded work (inv #1):** at most [`MAX_PACKETS_PER_POLL`] units are
    /// pulled per call. A malformed-packet flood — units that depacketize but
    /// never close a frame — is drained at that rate per tick rather than all at
    /// once, so one call (whether it runs on the output-sample side or a shared
    /// ingest thread) can never do unbounded work and delay the output clock
    /// (panel F1). Budget spent with no frame closed yields `Ok(None)`; the pump
    /// re-polls next tick.
    ///
    /// The bound is on packet **count**, not wall-clock time: it assumes each
    /// `poll_packet` + depacketize step is itself bounded, which the fixed
    /// ST 2110-20 depacketize is (its work is bounded by the datagram size).
    ///
    /// [`MAX_PACKETS_PER_POLL`]: St2110Producer::MAX_PACKETS_PER_POLL
    fn next_frame(&mut self) -> Result<Option<ProducedFrame>> {
        for _ in 0..Self::MAX_PACKETS_PER_POLL {
            let Some(packet) = self.source.poll_packet()? else {
                return Ok(None);
            };
            if let Some(frame) = self.push(&packet) {
                return Ok(Some(frame));
            }
        }
        // Budget spent on units that never closed a frame this tick: yield and
        // re-poll next tick so a malformed flood can never make one sample
        // unbounded work (inv #1).
        Ok(None)
    }

    fn timebase(&self) -> Rational {
        // ST 2110-20 video rides a 90 kHz RTP media clock.
        Rational::new(1, i64::from(VIDEO_CLOCK_RATE))
    }

    fn cadence(&self) -> Rational {
        // No cadence is carried in the RTP stream itself; assume 30 fps for the
        // genpts fallback (frames carry real 90 kHz timestamps in practice, so
        // this only matters for a timestamp-less packet, which does not occur in
        // ST 2110-20).
        Rational::new(30, 1)
    }

    fn wrap_bits(&self) -> WrapBits {
        WrapBits::Rtp32
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn sample_packet(sequence: u16) -> St2110Packet {
        St2110Packet {
            marker: false,
            timestamp: 0,
            sequence,
            ssrc: 0,
            payload_type: 96,
            payload: vec![0u8; 4],
        }
    }

    /// While the ring lock is held, a concurrent `poll_packet` on the ingest
    /// data plane must NOT block on it: it sheds (`Ok(None)`) and the caller
    /// re-polls next tick (inv #10 — the sync consumer never blocks behind the
    /// tokio producer). A blocking `.lock()` waits for the guard, so the result
    /// never arrives within the deadline.
    #[test]
    fn poll_packet_never_blocks_on_a_held_ring_lock() {
        let (sink, mut source) = ChannelPacketSource::bounded(4);
        let guard = sink.ring.lock().expect("uncontended lock");
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(source.poll_packet());
        });
        let received = rx.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let _ = worker.join();
        let outcome =
            received.expect("contended poll_packet must not block on the ring lock (inv #10)");
        assert!(
            matches!(outcome, Ok(None)),
            "a contended poll sheds to None, never a unit: {outcome:?}"
        );
    }

    /// While the ring lock is held, a concurrent `push` from the receive task
    /// must NOT block on it: it sheds the unit (never grows, never waits) and
    /// returns immediately (inv #10 — a stalled reader can never back-pressure
    /// the wire). A blocking `.lock()` waits for the guard.
    #[test]
    fn push_never_blocks_on_a_held_ring_lock() {
        let (sink, _source) = ChannelPacketSource::bounded(4);
        let guard = sink.ring.lock().expect("uncontended lock");
        let producer = sink.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            producer.push(sample_packet(1));
            let _ = tx.send(());
        });
        let received = rx.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let _ = worker.join();
        received.expect("contended push must not block on the ring lock (inv #10)");
    }
}
