//! The QUIC lane: an [`noq::AsyncUdpSocket`] that receives from the
//! dispatcher's bounded queues and sends straight to the shared socket.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use noq::udp::{RecvMeta, Transmit};

use crate::classify::SUPPORTED_QUIC_VERSIONS;
use crate::socket::SharedSocket;
use crate::LaneRx;

/// The endpoint-wide minimum interval between stateless resets (design note
/// section 3: one interval for the whole endpoint in noq, set explicitly).
pub const MIN_RESET_INTERVAL: Duration = Duration::from_millis(200);

/// The `noq::EndpointConfig` for an endpoint on the shared socket:
/// * `grease_quic_bit(false)`: RFC 9443 sections 1 and 3 require a
///   multiplexing endpoint not to advertise `grease_quic_bit` (RFC 9287), so
///   compliant peers keep the fixed bit set and never send 128-191;
/// * `min_reset_interval` 200 ms;
/// * the supported versions pinned to [`SUPPORTED_QUIC_VERSIONS`], the list
///   the dispatcher uses to tell Version Negotiation triggers apart.
#[must_use]
pub fn endpoint_config() -> noq::EndpointConfig {
    let mut config = noq::EndpointConfig::default();
    config
        .grease_quic_bit(false)
        .min_reset_interval(MIN_RESET_INTERVAL)
        .supported_versions(SUPPORTED_QUIC_VERSIONS.to_vec());
    config
}

/// Most handshakes noq holds before the server accepts them (the default is
/// 65 536). Each pending `Incoming` lives until the accept loop takes it.
pub const MAX_INCOMING: usize = 64;
/// Most bytes noq buffers for those pending handshakes (the default is 100 MiB).
pub const INCOMING_BUFFER_TOTAL: u64 = 4 * 1024 * 1024;

/// The `(max_incoming, incoming_buffer_size_total)` caps for
/// `moq_tokio::noq::Injected::incoming`.
#[must_use]
pub const fn incoming_caps() -> (usize, u64) {
    (MAX_INCOMING, INCOMING_BUFFER_TOTAL)
}

/// The QUIC consumer's view of the shared socket. Give it to
/// `noq::Endpoint::new_with_abstract_socket` (from inside the front door's
/// runtime, so the endpoint driver and its connections run there).
pub struct QuicLane {
    rx: Arc<Mutex<LaneRx>>,
    socket: Arc<SharedSocket>,
}

impl fmt::Debug for QuicLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicLane")
            .field("local", &self.socket.local_addr())
            .finish_non_exhaustive()
    }
}

impl QuicLane {
    pub(crate) fn new(rx: LaneRx, socket: Arc<SharedSocket>) -> Self {
        Self {
            rx: Arc::new(Mutex::new(rx)),
            socket,
        }
    }
}

impl noq::AsyncUdpSocket for QuicLane {
    fn create_sender(&self) -> Pin<Box<dyn noq::UdpSender>> {
        Box::pin(QuicSender {
            socket: Arc::clone(&self.socket),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Only the endpoint driver receives; the mutex is uncontended and is
        // never held across an await.
        let Ok(mut rx) = self.rx.lock() else {
            return Poll::Ready(Err(io::Error::other("quic lane poisoned")));
        };
        let first = match rx.poll_recv(cx) {
            Poll::Ready(Some(d)) => d,
            // Dispatcher gone: nothing arrives again; the endpoint stops
            // through its own close.
            Poll::Ready(None) | Poll::Pending => return Poll::Pending,
        };
        let mut filled = 0usize;
        let mut next = Some(first);
        while let Some(datagram) = next.take() {
            let (Some(buf), Some(slot)) = (bufs.get_mut(filled), meta.get_mut(filled)) else {
                break;
            };
            let len = datagram.payload.len();
            // noq sizes each buffer to its max UDP payload (1472 B by default).
            // A larger datagram cannot be a packet noq accepts; drop it rather
            // than hand noq a truncated one.
            let (Some(dst), Some(src)) = (buf.get_mut(..len), datagram.payload.get(..len)) else {
                next = rx.try_recv();
                continue;
            };
            dst.copy_from_slice(src);
            let mut m = RecvMeta::default();
            m.addr = datagram.src;
            m.len = len;
            // One datagram per buffer: noq walks `len` in `stride` steps
            // (`split_to(stride)`), so stride == len hands it exactly one
            // datagram. stride 0 would never advance.
            m.stride = len;
            m.ecn = datagram.ecn;
            m.dst_ip = datagram.dst_ip;
            *slot = m;
            filled += 1;
            if filled < bufs.len().min(meta.len()) {
                next = rx.try_recv();
            }
        }
        Poll::Ready(Ok(filled))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.socket.local_addr())
    }

    fn max_receive_segments(&self) -> std::num::NonZeroUsize {
        // The dispatcher already split GRO batches into single datagrams.
        std::num::NonZeroUsize::MIN
    }

    fn may_fragment(&self) -> bool {
        self.socket.may_fragment()
    }
}

/// Sends straight to the shared socket (GSO where available), bypassing the
/// dispatcher.
struct QuicSender {
    socket: Arc<SharedSocket>,
}

impl fmt::Debug for QuicSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QuicSender")
    }
}

impl noq::UdpSender for QuicSender {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        self.socket.poll_send(transmit, cx)
    }

    fn max_transmit_segments(&self) -> std::num::NonZeroUsize {
        self.socket.max_gso_segments()
    }
}
