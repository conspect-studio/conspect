//! The AES67 / ST 2110-30 UDP **send transport** — feature `aes67`.
//!
//! The thin, off-hot-path tokio `UdpSocket` layer that carries the
//! [`Aes67Sender`](super::sender::Aes67Sender)'s continuous marker=0 RTP packets
//! onto the wire. It owns only the socket and the destination; the packetization
//! (the pure [`super::packet`] / [`super::sender`]) is what carries the
//! correctness load and is tested with no NIC.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! [`Aes67UdpSender::serve`] runs on its **own** timer (never the output-clock
//! loop), draining one packet per `ptime` tick and `send_to`-ing it. It samples
//! the sender's bounded drop-oldest FIFO — a stalled socket can only drop the
//! oldest buffered frames, never back-pressure the engine. `send_to` never runs
//! on the output-clock loop.
//!
//! IPv6-first (ADR-0042): bind a dual-stack or IPv6 local address and send to a
//! bracketed IPv6 group for real multicast; loopback validation uses `[::1]` /
//! `127.0.0.1`. Multicast group join + TTL/hop-limit 255 for a real deployment
//! is wired by the engine at bind time (the send socket needs no group
//! membership); this transport carries the datagrams.

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use crate::error::{Error, Result};

use super::sender::Aes67Sender;

/// Which network interface to send IPv6 multicast on (`IPV6_MULTICAST_IF`).
///
/// IPv6 multicast is **interface-scoped** (RFC 4291 scope-id): the OS index `0`
/// ("unspecified" — let the kernel pick) is not portable and commonly fails for
/// **link-local** (`ff02::/16`) / **site-local** (`ff05::/16`) AES67 groups, which
/// must name a concrete interface. A deployment supplies the intended interface
/// (from config or the SDP scope), so TX egress is not silently pinned to index 0
/// (panel F8). IPv4 egress uses the multicast TTL.
///
/// This mirrors `multiview_input::st2110::transport::MulticastInterface`; the input
/// and output crates are intentionally decoupled (neither depends on the other),
/// so the small type is defined in each rather than shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MulticastInterface {
    /// Let the OS pick the default multicast interface (IPv6 index `0`).
    /// Portable for a global-scope group on a single-homed host; NOT reliable for
    /// link/site-local groups or a multi-homed host.
    #[default]
    Unspecified,
    /// A specific IPv6 interface by its OS index / scope-id (resolved from an
    /// interface name or supplied by config). Required for link/site-local groups
    /// and to pin egress on a multi-homed host.
    Index(u32),
}

impl MulticastInterface {
    /// The IPv6 interface index this selection passes to `IPV6_MULTICAST_IF`: `0`
    /// for [`Unspecified`](Self::Unspecified), else the explicit index.
    #[must_use]
    pub const fn index(self) -> u32 {
        match self {
            MulticastInterface::Unspecified => 0,
            MulticastInterface::Index(index) => index,
        }
    }
}

/// A bound UDP send socket for one AES67 / ST 2110-30 flow.
#[derive(Debug)]
pub struct Aes67UdpSender {
    socket: UdpSocket,
    dest: SocketAddr,
    /// The IPv6 multicast egress interface (panel F8); unspecified (OS default)
    /// unless set via [`with_interface`](Self::with_interface).
    interface: MulticastInterface,
}

impl Aes67UdpSender {
    /// Bind a send socket to `local` and target `dest` (the multicast group +
    /// port, or a unicast peer for loopback validation).
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr, dest: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Output(format!("aes67 bind {local}: {e}")))?;
        Ok(Self {
            socket,
            dest,
            interface: MulticastInterface::Unspecified,
        })
    }

    /// The destination this sender targets.
    #[must_use]
    pub const fn dest(&self) -> SocketAddr {
        self.dest
    }

    /// Set the IPv6 multicast **egress** interface for a real deployment (default
    /// [`MulticastInterface::Unspecified`] → the OS default). Applied by
    /// [`configure_multicast_egress`](Self::configure_multicast_egress).
    #[must_use]
    pub fn with_interface(mut self, interface: MulticastInterface) -> Self {
        self.interface = interface;
        self
    }

    /// Select the IPv6 multicast egress interface (`IPV6_MULTICAST_IF`) and set
    /// hop-limit 255 (`IPV6_MULTICAST_HOPS`; ADR-0033 §5 / ADR-0041 §7) on the send
    /// socket. Call once before serving on a scoped-multicast IPv6 dest; family-
    /// wise a no-op on a v4 socket (whose multicast TTL is set via
    /// [`set_multicast_ttl_v4`](Self::set_multicast_ttl_v4)).
    ///
    /// The egress interface index comes from config or the SDP scope and is wired
    /// by the pipeline (#103); until then it defaults to the OS default rather than
    /// a hardcoded `0`. **Live validation on a real IPv6 multicast network is
    /// hardware-gated** (rule 26).
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if the local address cannot be read or the OS rejects the
    /// egress interface / hop-limit (e.g. a nonexistent interface index).
    pub fn configure_multicast_egress(&self) -> Result<()> {
        let local = self
            .socket
            .local_addr()
            .map_err(|e| Error::Output(format!("aes67 local_addr: {e}")))?;
        if local.is_ipv6() {
            let sock = socket2::SockRef::from(&self.socket);
            sock.set_multicast_if_v6(self.interface.index())
                .map_err(|e| Error::Output(format!("aes67 set multicast egress if v6: {e}")))?;
            // AES67 / ST 2110-30 use hop-limit 255 (scope is carried by the group
            // address, not the hop-limit).
            sock.set_multicast_hops_v6(255)
                .map_err(|e| Error::Output(format!("aes67 set multicast hops v6: {e}")))?;
        }
        Ok(())
    }

    /// Set the IPv4 multicast TTL for a real multicast deployment (ADR-0033 §5 /
    /// ADR-0041 §7 use TTL 255). No-op for a unicast loopback destination.
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if the socket option cannot be set.
    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> Result<()> {
        self.socket
            .set_multicast_ttl_v4(ttl)
            .map_err(|e| Error::Output(format!("aes67 set multicast ttl: {e}")))
    }

    /// Send one already-built RTP packet to the destination.
    ///
    /// # Errors
    ///
    /// [`Error::Output`] on a socket send failure.
    pub async fn send_packet(&self, packet: &[u8]) -> Result<()> {
        self.socket
            .send_to(packet, self.dest)
            .await
            .map(|_sent| ())
            .map_err(|e| Error::Output(format!("aes67 send_to {}: {e}", self.dest)))
    }

    /// Drive `sender` on its **media-clock cadence**, sending one continuous
    /// marker=0 packet per tick until `stop` resolves or the socket faults.
    ///
    /// Each packet is sent at its **absolute** deadline
    /// [`sender.packet_deadline_offset(n)`](Aes67Sender::packet_deadline_offset)
    /// from the loop start — `n × frames_per_packet / sample_rate` — via
    /// `sleep_until`, **not** a repeated interval. Deriving every deadline from the
    /// cumulative packet index keeps the wire cadence locked to the RTP media clock
    /// (which advances `+frames_per_packet` per packet) with sub-nanosecond error
    /// forever; repeating one floored `packet_duration` would accumulate the
    /// truncation into unbounded drift at any non-dividing rate and walk the
    /// receiver buffer (panel T1, inv #1/#3).
    ///
    /// This is the off-hot-path send loop: it samples the sender's bounded
    /// drop-oldest FIFO and never paces the engine (invariants #1 / #10). The
    /// engine feeds the program bus in through an
    /// [`Aes67SenderHandle`](super::sender::Aes67SenderHandle::push); this loop
    /// only drains and transmits.
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if a `send_to` fails (the supervisor reconnects).
    pub async fn serve<S>(&self, sender: &mut Aes67Sender, stop: S) -> Result<()>
    where
        S: std::future::Future<Output = ()>,
    {
        tokio::pin!(stop);
        // Select the IPv6 multicast egress interface (IPV6_MULTICAST_IF + hop-limit
        // 255) on the send socket BEFORE the first packet (panel S1): F8 wired
        // `configure_multicast_egress` but nothing on the serve path called it, so a
        // configured interface was silently ignored and egress fell to the OS
        // default. Unspecified (the default until #103 supplies the index) configures
        // cleanly; a misconfigured interface fails fast here rather than streaming on
        // the wrong egress. A no-op on a v4 socket.
        self.configure_multicast_egress()?;
        // Absolute per-packet deadlines from one fixed start (panel T1): packet n
        // is due at start + n×frames_per_packet/sample_rate, floored ONCE from the
        // cumulative index so the rounding never accumulates into drift the way a
        // repeated truncated interval would. `sleep_until` each deadline.
        let start = tokio::time::Instant::now();
        // One datagram buffer, warmed on the first tick and reused for every
        // packet — the continuous send path allocates nothing per tick (rule 22 /
        // panel F6).
        let mut datagram = Vec::new();
        let mut packet_index: u64 = 0;
        loop {
            let deadline = start + sender.packet_deadline_offset(packet_index);
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    sender.next_packet_into(&mut datagram);
                    self.send_packet(&datagram).await?;
                    packet_index = packet_index.saturating_add(1);
                }
                () = &mut stop => return Ok(()),
            }
        }
    }
}
