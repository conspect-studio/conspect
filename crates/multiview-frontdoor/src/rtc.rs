//! The WebRTC lane: inbound datagrams from the dispatcher's bounded queues, a
//! send handle on the same socket (a duplicated descriptor of the shared socket,
//! so the WebRTC driver's existing non-blocking sends are unchanged), and the
//! established set the driver writes after ICE validation.

use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::limits::EstablishedSet;
use crate::socket::SharedSocket;
use crate::{Datagram, LaneRx};

/// The WebRTC consumer's view of the shared socket.
#[derive(Debug)]
pub struct RtcLane {
    rx: LaneRx,
    socket: Arc<SharedSocket>,
    established: Arc<EstablishedSet>,
}

impl RtcLane {
    pub(crate) fn new(
        rx: LaneRx,
        socket: Arc<SharedSocket>,
        established: Arc<EstablishedSet>,
    ) -> Self {
        Self {
            rx,
            socket,
            established,
        }
    }

    /// The shared socket's bound address (`[::]:port`).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// A duplicate descriptor of the shared socket for sending (same port, same
    /// kernel socket). The consumer must only send on it: every receive goes
    /// through the dispatcher.
    ///
    /// # Errors
    ///
    /// The `dup` error.
    pub fn send_socket(&self) -> std::io::Result<std::net::UdpSocket> {
        self.socket.try_clone_std()
    }

    /// Poll for the next datagram, established peers first.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Datagram>> {
        self.rx.poll_recv(cx)
    }

    /// Take a queued datagram without waiting.
    pub fn try_recv(&mut self) -> Option<Datagram> {
        self.rx.try_recv()
    }

    /// Wait for the next datagram.
    pub async fn recv(&mut self) -> Option<Datagram> {
        self.rx.recv().await
    }

    /// The established set this lane's consumer writes.
    #[must_use]
    pub fn established(&self) -> &Arc<EstablishedSet> {
        &self.established
    }
}

#[cfg(feature = "webrtc")]
mod webrtc_adapter {
    use super::RtcLane;
    use multiview_webrtc::transport::{InboundDatagram, SharedInbound};
    use std::net::SocketAddr;
    use std::task::{Context, Poll};

    fn convert(d: crate::Datagram) -> InboundDatagram {
        // The source stays as the dual-stack socket reported it (IPv4 peers
        // IPv4-mapped), exactly what the socket-owning driver hands str0m, so
        // replies go back through the same AF_INET6 socket unchanged.
        InboundDatagram {
            src: d.src,
            dst_ip: d.dst_ip,
            payload: d.payload,
        }
    }

    impl SharedInbound for RtcLane {
        fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<InboundDatagram>> {
            self.poll_recv(cx).map(|d| d.map(convert))
        }

        fn try_next(&mut self) -> Option<InboundDatagram> {
            self.try_recv().map(convert)
        }

        fn validated(&mut self, peer: SocketAddr) -> bool {
            self.established.insert(peer)
        }

        fn invalidated(&mut self, peer: SocketAddr) {
            self.established.remove(peer);
        }
    }
}
