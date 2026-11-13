//! The shared dual-stack UDP socket: batched receive (recvmmsg + GRO through
//! `noq::udp::UdpSocketState`), non-blocking send (GSO), no `unsafe`.

use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use noq::udp::{EcnCodepoint, RecvMeta, Transmit, UdpSocketState};

/// The largest UDP payload a segment can carry (Ethernet MTU minus IP/UDP).
const MAX_SEGMENT: usize = 1472;

/// The socket, its batching state, and the wakers of senders waiting for it to
/// become writable.
#[derive(Debug)]
pub struct SharedSocket {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
    local: SocketAddr,
    blocked: Arc<Blocked>,
}

impl SharedSocket {
    /// Bind `addr` (must be IPv6; bound with `IPV6_V6ONLY=false`) in the
    /// current runtime's reactor.
    pub(crate) fn bind(addr: SocketAddr, recv_buffer_bytes: usize) -> io::Result<Self> {
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
        if recv_buffer_bytes > 0 {
            if let Err(e) = socket.set_recv_buffer_size(recv_buffer_bytes) {
                tracing::warn!(error = %e, "front door: SO_RCVBUF request failed");
            }
        }
        socket.bind(&addr.into())?;
        Self::adopt(socket.into(), recv_buffer_bytes)
    }

    /// Adopt an already-bound UDP socket (systemd socket activation, slice 6):
    /// made non-blocking and registered in the current runtime's reactor.
    /// `SO_RCVBUF` is left as the service manager set it (`ReceiveBuffer=`);
    /// the clamp warning below still reports a smaller grant.
    pub(crate) fn from_std(
        std_socket: std::net::UdpSocket,
        recv_buffer_bytes: usize,
    ) -> io::Result<Self> {
        let socket = socket2::Socket::from(std_socket);
        if !matches!(socket.local_addr()?.as_socket(), Some(SocketAddr::V6(_))) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an inherited front-door UDP socket must be IPv6 (dual-stack [::]:443)",
            ));
        }
        if socket.only_v6().unwrap_or(false) {
            tracing::warn!(
                "front door: the inherited UDP socket is IPv6-only (BindIPv6Only=both missing \
                 from the socket unit); IPv4 clients cannot reach it"
            );
        }
        socket.set_nonblocking(true)?;
        Self::adopt(socket.into(), recv_buffer_bytes)
    }

    fn adopt(std_socket: std::net::UdpSocket, recv_buffer_bytes: usize) -> io::Result<Self> {
        // Enables GRO, PKTINFO and ECN reporting on the socket.
        let state = UdpSocketState::new((&std_socket).into())?;
        let local = std_socket.local_addr()?;
        let io = tokio::net::UdpSocket::from_std(std_socket)?;
        let shared = Self {
            io,
            state,
            local,
            blocked: Arc::default(),
        };
        if let Ok(granted) = shared.recv_buffer_size() {
            // Linux reports double the usable size.
            if recv_buffer_bytes > 0 && granted < recv_buffer_bytes {
                tracing::warn!(
                    requested = recv_buffer_bytes,
                    granted,
                    "front door: the kernel clamped SO_RCVBUF (net.core.rmem_max); a flood \
                     overflows the kernel queue sooner"
                );
            }
        }
        Ok(shared)
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

    pub(crate) async fn readable(&self) -> io::Result<()> {
        self.io.readable().await
    }

    /// One non-blocking batched read into `batch`. `Ok(0)` is never returned by
    /// the kernel for a datagram socket; `WouldBlock` means drained.
    pub(crate) fn try_recv_batch(&self, batch: &mut RecvBatch) -> io::Result<usize> {
        self.io.try_io(tokio::io::Interest::READABLE, || {
            let mut slices: Vec<IoSliceMut<'_>> = batch
                .bufs
                .iter_mut()
                .map(|b| IoSliceMut::new(b.as_mut_slice()))
                .collect();
            self.state
                .recv((&self.io).into(), &mut slices, &mut batch.meta)
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
        let per_buf = MAX_SEGMENT.saturating_mul(socket.state.gro_segments().get().max(1));
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

/// Wakers of senders blocked on a full send buffer.
#[derive(Debug, Default)]
struct Blocked(Mutex<Vec<Waker>>);

impl Blocked {
    fn push(&self, waker: &Waker) {
        if let Ok(mut wakers) = self.0.lock() {
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }
        }
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
