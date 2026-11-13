//! The shared dual-stack UDP socket(s): batched receive (recvmmsg + GRO
//! through `noq::udp::UdpSocketState`), non-blocking send (GSO). With
//! validated-flow steering (Linux) there are two sockets in one
//! `SO_REUSEPORT` group: the listener (index 0, every unvalidated datagram,
//! and every send) and the established socket (index 1, the steered peers).
//! The only `unsafe` behind this module lives in `crate::sys`.

use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use noq::udp::{EcnCodepoint, RecvMeta, Transmit, UdpSocketState};
use smallvec::SmallVec;

use crate::steering::{ProgramSet, ReuseportGroup, SteeringMode};

/// The largest UDP payload a segment can carry (Ethernet MTU minus IP/UDP).
const MAX_SEGMENT: usize = 1472;

/// Inline capacity of the per-call `IoSliceMut` slice built in
/// [`SharedSocket::try_recv_batch`]: every platform's `noq::udp::BATCH_SIZE`
/// (32 on Linux, 1 elsewhere) fits without spilling to the heap.
const RECV_BATCH_INLINE: usize = 32;

/// What the kernel granted for one socket's receive buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecvBufferReport {
    /// The request (`FrontDoorConfig::recv_buffer_bytes`); 0 = kernel default.
    pub requested: usize,
    /// What `getsockopt(SO_RCVBUF)` reads back (Linux: double the usable size).
    pub granted: usize,
    /// The usable size: `granted / 2` on Linux, `granted` elsewhere.
    pub usable: usize,
    /// `usable < requested`: the kernel clamped the request
    /// (`net.core.rmem_max`, and `SO_RCVBUFFORCE` was not permitted).
    pub clamped: bool,
}

impl RecvBufferReport {
    /// The smaller of two reports (the pair is only as deep as its shallower
    /// queue).
    #[must_use]
    pub fn min(self, other: Self) -> Self {
        if other.usable < self.usable {
            other
        } else {
            self
        }
    }
}

/// `net.core.rmem_max`, when readable.
fn rmem_max() -> Option<usize> {
    std::fs::read_to_string("/proc/sys/net/core/rmem_max")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Request `bytes` of receive buffer: `SO_RCVBUFFORCE` first on Linux (works
/// with `CAP_NET_ADMIN`), then `SO_RCVBUF`; read back and report.
fn size_recv_buffer(socket: &socket2::Socket, bytes: usize) -> RecvBufferReport {
    if bytes > 0 {
        #[cfg(target_os = "linux")]
        let forced = {
            use std::os::fd::AsFd;
            crate::sys::set_recv_buffer_force(socket.as_fd(), bytes).is_ok()
        };
        #[cfg(not(target_os = "linux"))]
        let forced = false;
        if !forced {
            if let Err(e) = socket.set_recv_buffer_size(bytes) {
                tracing::warn!(error = %e, "front door: SO_RCVBUF request failed");
            }
        }
    }
    let granted = socket.recv_buffer_size().unwrap_or(0);
    let usable = if cfg!(target_os = "linux") {
        granted / 2
    } else {
        granted
    };
    let report = RecvBufferReport {
        requested: bytes,
        granted,
        usable,
        clamped: bytes > 0 && usable < bytes,
    };
    if report.clamped {
        tracing::warn!(
            requested = bytes,
            granted,
            usable,
            rmem_max = rmem_max().unwrap_or(0),
            "front door: the kernel clamped the UDP receive buffer; a flood overflows \
             the kernel queue sooner. Fix: let systemd pass the socket with \
             ReceiveBuffer= (SO_RCVBUFFORCE), grant CAP_NET_ADMIN, or raise \
             net.core.rmem_max (docs/runbooks/udp-443-flood.md)"
        );
    }
    report
}

/// A dual-stack (`IPV6_V6ONLY=false`), non-blocking UDP socket bound to
/// `addr` (must be IPv6), optionally with `SO_REUSEPORT`.
pub(crate) fn new_udp_socket(
    addr: SocketAddr,
    reuse_port: bool,
    recv_buffer_bytes: usize,
) -> io::Result<(std::net::UdpSocket, RecvBufferReport)> {
    use socket2::{Domain, Protocol, Socket, Type};
    if !addr.is_ipv6() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the front door binds [::] dual-stack, never an IPv4 address",
        ));
    }
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.set_nonblocking(true)?;
    if reuse_port {
        #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
        socket.set_reuse_port(true)?;
        #[cfg(not(all(unix, not(any(target_os = "solaris", target_os = "illumos")))))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SO_REUSEPORT unsupported",
        ));
    }
    let report = size_recv_buffer(&socket, recv_buffer_bytes);
    socket.bind(&addr.into())?;
    Ok((socket.into(), report))
}

/// The steered peers' socket (group index 1): receive only.
#[derive(Debug)]
struct EstablishedSocket {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
}

/// Which socket of the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Which {
    Listener,
    Established,
}

/// The socket(s), batching state, and the wakers of senders waiting for the
/// listener to become writable.
#[derive(Debug)]
pub struct SharedSocket {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
    local: SocketAddr,
    blocked: Arc<Blocked>,
    established: Option<EstablishedSocket>,
    mode: SteeringMode,
    buffers: RecvBufferReport,
}

impl SharedSocket {
    /// Bind `addr` (must be IPv6; bound with `IPV6_V6ONLY=false`) in the
    /// current runtime's reactor. With `steering`, try the reuseport pair
    /// first and fall back to one socket on any error (recorded in
    /// [`mode`](Self::mode)).
    pub(crate) fn bind(
        addr: SocketAddr,
        recv_buffer_bytes: usize,
        steering: bool,
    ) -> io::Result<Self> {
        if steering {
            match ReuseportGroup::bind(addr, recv_buffer_bytes) {
                Ok(group) => {
                    let (listener, established, [lb, eb]) = group.into_parts();
                    let est_state = UdpSocketState::new((&established).into())?;
                    let est = EstablishedSocket {
                        io: tokio::net::UdpSocket::from_std(established)?,
                        state: est_state,
                    };
                    return Self::from_listener(
                        listener,
                        Some(est),
                        SteeringMode::ReuseportCbpf,
                        lb.min(eb),
                    );
                }
                Err(e) => {
                    let (listener, report) = new_udp_socket(addr, false, recv_buffer_bytes)?;
                    return Self::from_listener(
                        listener,
                        None,
                        SteeringMode::SingleSocket {
                            reason: format!("reuseport steering unavailable: {e}"),
                        },
                        report,
                    );
                }
            }
        }
        let (listener, report) = new_udp_socket(addr, false, recv_buffer_bytes)?;
        Self::from_listener(
            listener,
            None,
            SteeringMode::SingleSocket {
                reason: "disabled by configuration (steering = false)".to_owned(),
            },
            report,
        )
    }

    fn from_listener(
        std_socket: std::net::UdpSocket,
        established: Option<EstablishedSocket>,
        mode: SteeringMode,
        buffers: RecvBufferReport,
    ) -> io::Result<Self> {
        // Enables GRO, PKTINFO and ECN reporting on the socket.
        let state = UdpSocketState::new((&std_socket).into())?;
        let local = std_socket.local_addr()?;
        let io = tokio::net::UdpSocket::from_std(std_socket)?;
        Ok(Self {
            io,
            state,
            local,
            blocked: Arc::default(),
            established,
            mode,
            buffers,
        })
    }

    /// Replace the single-socket reason (the caller knows better).
    #[must_use]
    pub(crate) fn with_reason(mut self, reason: String) -> Self {
        if matches!(self.mode, SteeringMode::SingleSocket { .. }) {
            self.mode = SteeringMode::SingleSocket { reason };
        }
        self
    }

    /// How this socket receives.
    #[must_use]
    pub fn mode(&self) -> &SteeringMode {
        &self.mode
    }

    /// The receive-buffer report (the smaller of the pair's).
    #[must_use]
    pub fn buffer_report(&self) -> RecvBufferReport {
        self.buffers
    }

    /// Whether the established socket exists.
    pub(crate) fn has_established(&self) -> bool {
        self.established.is_some()
    }

    /// Attach (replace) the steering program. Never called by the dispatcher.
    pub(crate) fn attach(&self, set: &ProgramSet) -> io::Result<()> {
        if self.established.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "single socket: nothing to steer",
            ));
        }
        crate::steering::attach_to(&self.io, set)
    }

    /// The socket inodes (listener, established) for the kernel-drop sampler.
    pub(crate) fn inodes(&self) -> (Option<u64>, Option<u64>) {
        (
            socket_inode(&self.io),
            self.established.as_ref().and_then(|e| socket_inode(&e.io)),
        )
    }

    /// The bound address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The receive buffer the kernel granted.
    ///
    /// # Errors
    ///
    /// The `getsockopt` error.
    pub fn recv_buffer_size(&self) -> io::Result<usize> {
        self.state.recv_buffer_size((&self.io).into())
    }

    /// A duplicate descriptor of the socket (for a consumer that only sends).
    pub(crate) fn try_clone_std(&self) -> io::Result<std::net::UdpSocket> {
        let sock = socket2::SockRef::from(&self.io).try_clone()?;
        sock.set_nonblocking(true)?;
        Ok(sock.into())
    }

    /// Wait until either socket is readable.
    pub(crate) async fn readable(&self) -> io::Result<()> {
        match &self.established {
            None => self.io.readable().await,
            Some(est) => {
                tokio::select! {
                    biased;
                    r = est.io.readable() => r,
                    r = self.io.readable() => r,
                }
            }
        }
    }

    /// One non-blocking batched read from `which` into `batch`. `WouldBlock`
    /// means drained (a missing established socket is always drained).
    pub(crate) fn try_recv_batch(&self, which: Which, batch: &mut RecvBatch) -> io::Result<usize> {
        let (io, state) = match which {
            Which::Listener => (&self.io, &self.state),
            Which::Established => match &self.established {
                Some(e) => (&e.io, &e.state),
                None => return Err(io::ErrorKind::WouldBlock.into()),
            },
        };
        io.try_io(tokio::io::Interest::READABLE, || {
            // `slices` is a `SmallVec` sized to `RECV_BATCH_INLINE` (>= every
            // platform's `noq::udp::BATCH_SIZE`), so rebuilding the borrowed
            // `IoSliceMut`s into `batch.bufs` each call stays on the stack:
            // no heap allocation per batch (the per-call `Vec<IoSliceMut>`
            // this replaced always allocated).
            let mut slices: SmallVec<[IoSliceMut<'_>; RECV_BATCH_INLINE]> = SmallVec::new();
            slices.extend(
                batch
                    .bufs
                    .iter_mut()
                    .map(|b| IoSliceMut::new(b.as_mut_slice())),
            );
            state.recv(io.into(), &mut slices, &mut batch.meta)
        })
    }

    /// Send without waiting: `WouldBlock` (a full send buffer) is returned to the
    /// caller, who drops the datagram (UDP may always lose it).
    pub(crate) fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        self.io.try_io(tokio::io::Interest::WRITABLE, || {
            self.state.send((&self.io).into(), transmit)
        })
    }

    /// Send, registering `cx` to be woken when the socket becomes writable.
    /// Every QUIC connection sends from its own task while tokio keeps one
    /// writable waker, so waiting senders are fanned out (as the vendored
    /// moq-tokio socket does).
    pub(crate) fn poll_send(
        &self,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match self.try_send(transmit) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                res => return Poll::Ready(res),
            }
            self.blocked.push(cx.waker());
            let waker = Waker::from(Arc::clone(&self.blocked));
            std::task::ready!(self.io.poll_send_ready(&mut Context::from_waker(&waker)))?;
        }
    }

    pub(crate) fn max_gso_segments(&self) -> std::num::NonZeroUsize {
        self.state.max_gso_segments()
    }

    pub(crate) fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
}

/// Receive buffers and metadata for one batched read, allocated once.
#[derive(Debug)]
pub(crate) struct RecvBatch {
    bufs: Vec<Vec<u8>>,
    meta: Vec<RecvMeta>,
}

impl RecvBatch {
    pub(crate) fn new(socket: &SharedSocket) -> Self {
        let n = noq::udp::BATCH_SIZE.max(1);
        // Each buffer must hold the largest GRO list the kernel may return
        // (`gro_segments` x segment size, up to UDP_GRO_CNT_MAX = 64): noq-udp's
        // recvmmsg path does not report MSG_TRUNC, so a smaller buffer would
        // silently cut a sender's tail segment (review PR 315).
        let gro = socket
            .established
            .as_ref()
            .map_or(1, |e| e.state.gro_segments().get())
            .max(socket.state.gro_segments().get())
            .max(1);
        let per_buf = MAX_SEGMENT.saturating_mul(gro);
        Self {
            bufs: (0..n).map(|_| vec![0u8; per_buf]).collect(),
            meta: vec![RecvMeta::default(); n],
        }
    }

    /// The datagrams of the first `n` messages, with GRO super-datagrams split
    /// at their stride.
    pub(crate) fn datagrams(
        &self,
        n: usize,
    ) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, Option<EcnCodepoint>, &[u8])> + '_ {
        self.meta
            .iter()
            .zip(self.bufs.iter())
            .take(n)
            .flat_map(|(meta, buf)| {
                let data = buf.get(..meta.len.min(buf.len())).unwrap_or(&[]);
                let stride = if meta.stride == 0 {
                    data.len().max(1)
                } else {
                    meta.stride
                };
                data.chunks(stride)
                    .map(move |chunk| (meta.addr, meta.dst_ip, meta.ecn, chunk))
            })
    }
}

/// Most wakers held for senders blocked on a full send buffer (one per QUIC
/// connection task plus the WebRTC driver; the default connection cap is
/// [`crate::DEFAULT_MAX_QUIC_PEERS`]).
pub(crate) const MAX_BLOCKED_WAKERS: usize = 1024;

/// Wakers of senders blocked on a full send buffer, at most
/// [`MAX_BLOCKED_WAKERS`]. A full list is never grown and never loses a
/// waker: every held waker is woken (its sender simply polls again) and the
/// list restarts with the new one.
#[derive(Debug, Default)]
struct Blocked(Mutex<Vec<Waker>>);

impl Blocked {
    fn push(&self, waker: &Waker) {
        let overflow = {
            let Ok(mut wakers) = self.0.lock() else {
                // Poisoned: wake at once rather than park the sender forever.
                waker.wake_by_ref();
                return;
            };
            if wakers.iter().any(|w| w.will_wake(waker)) {
                return;
            }
            if wakers.len() >= MAX_BLOCKED_WAKERS {
                let held = std::mem::take(&mut *wakers);
                wakers.push(waker.clone());
                held
            } else {
                wakers.push(waker.clone());
                Vec::new()
            }
        };
        // Woken outside the lock (a waker may run arbitrary code).
        for w in overflow {
            w.wake();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().map_or(0, |w| w.len())
    }
}

impl std::task::Wake for Blocked {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let wakers = match self.0.lock() {
            Ok(mut w) => std::mem::take(&mut *w),
            Err(_) => return,
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

/// The inode of a socket (`/proc/self/fd/<fd>` -> `socket:[<inode>]`).
#[cfg(target_os = "linux")]
fn socket_inode(socket: &impl std::os::fd::AsRawFd) -> Option<u64> {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd())).ok()?;
    let link = link.to_str()?;
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn socket_inode<T>(_socket: &T) -> Option<u64> {
    None
}

/// The kernel receive-drop count of `socket` (Linux `/proc/net/udp6`, where
/// dual-stack IPv6 sockets are listed). `None` off Linux or when not found.
#[must_use]
pub fn kernel_drops(socket: &std::net::UdpSocket) -> Option<u64> {
    let inode = socket_inode(socket)?;
    let table = std::fs::read_to_string("/proc/net/udp6").ok()?;
    parse_udp_drops(&table, inode)
}

/// Whether [`kernel_drops`] can work on this platform.
#[must_use]
pub const fn kernel_drops_supported() -> bool {
    cfg!(target_os = "linux")
}

/// The `drops` column (the last) of the `/proc/net/udp6` row whose inode
/// (column 10) is `inode`.
#[must_use]
pub fn parse_udp_drops(table: &str, inode: u64) -> Option<u64> {
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let row_inode: u64 = fields.clone().nth(9)?.parse().ok()?;
        if row_inode != inode {
            return None;
        }
        fields.next_back()?.parse().ok()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const FIXTURE: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
 7609: 00000000000000000000000000000000:20A1 00000000000000000000000000000000:0000 07 00000000:00000000 00:00000000 00000000  1001        0 1092622191 2 0000000000000000 0
40803: 00000000000000000000000000000000:A24B 00000000000000000000000000000000:0000 07 00000000:00034100 00:00000000 00000000  1001        0 376840788 2 0000000000000000 4711
";

    #[test]
    fn drops_are_the_last_column_of_the_matching_inode() {
        assert_eq!(parse_udp_drops(FIXTURE, 376_840_788), Some(4711));
        assert_eq!(parse_udp_drops(FIXTURE, 1_092_622_191), Some(0));
        assert_eq!(parse_udp_drops(FIXTURE, 1), None);
        assert_eq!(parse_udp_drops("", 1), None);
        assert_eq!(parse_udp_drops("header\n garbage line", 1), None);
    }

    struct Count(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn blocked_wakers_are_capped_and_none_is_lost() {
        let blocked = Arc::new(Blocked::default());
        let counters: Vec<Arc<Count>> = (0..3 * MAX_BLOCKED_WAKERS + 7)
            .map(|_| Arc::new(Count(std::sync::atomic::AtomicUsize::new(0))))
            .collect();
        for c in &counters {
            blocked.push(&Waker::from(Arc::clone(c)));
            assert!(blocked.len() <= MAX_BLOCKED_WAKERS);
        }
        // The socket becomes writable: everything still held is woken.
        Waker::from(Arc::clone(&blocked)).wake();
        assert_eq!(blocked.len(), 0);
        for (i, c) in counters.iter().enumerate() {
            assert_eq!(
                c.0.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "waker {i} woken exactly once"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_socket_is_found_in_proc_net_udp6() {
        let (s, report) = new_udp_socket("[::1]:0".parse().unwrap(), false, 65_536).unwrap();
        assert_eq!(kernel_drops(&s), Some(0));
        assert_eq!(report.usable * 2, report.granted);
        assert_eq!(report.requested, 65_536);
    }
}
