//! The SAP **socket transport** — the supervised UDP listener + announcer
//! (RFC 2974 §3/§5; ADR-0041 §3/§5). **Feature `st2110`** (a `tokio` `UdpSocket`,
//! no FFI — the crate stays `unsafe_code = forbid`, LGPL-clean).
//!
//! [`SapListener`] binds an IPv6-first UDP socket, (on a real network) joins the
//! SAP group set, and runs a receive loop that parses each datagram and folds
//! announcements into the wait-free [`SapSessionTable`]. A malformed/rejected
//! datagram is skipped — the loop never dies on bad input; a spoofed inbound
//! deletion never withdraws a tracked session (the table ignores `T=1`,
//! ADR-0041 §8). [`SapAnnouncer`] builds and sends the `T=0`/`T=1` packets on an
//! **independent timer** ([`AnnounceSchedule`]).
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Everything here is control/discovery plane: the receive loop folds into a
//! **bounded, drop-oldest, wait-free** table (no unbounded queue anywhere), the
//! announce timer is independent of the per-tick output loop, and nothing can
//! block or back-pressure the engine. The multicast group join is the live
//! deployment path (this devcontainer's loopback is not multicast-capable), so
//! the tests exercise the sockets over unicast loopback.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
// A tokio time source so the purge cadence and the session-age `now` share one
// clock and are testable under `tokio::time` pause; in production this is real
// monotonic time, identical in behaviour to `std::time::Instant`.
use tokio::time::Instant;

use crate::error::{Error, Result};

use crate::st2110::transport::MulticastInterface;

use super::announce::{announcement, deletion, AnnounceSchedule};
use super::groups::{receive_group_set, SAP_TTL};
use super::packet::SapPacket;
use super::ratelimit::{SapRateLimiter, DEFAULT_ACCEPT_BURST, DEFAULT_ACCEPT_WINDOW};
use super::session::SapSessionTable;

/// The largest SAP datagram accepted (the max UDP payload). The packet parser
/// separately caps the SDP body, so a large datagram cannot force an unbounded
/// allocation.
pub const MAX_SAP_DATAGRAM: usize = 65_536;

/// How often [`SapListener::run`] purges expired sessions. Purge is decoupled
/// from per-datagram work — sessions age on the scale of seconds to an hour — so
/// a datagram flood cannot amplify the O(n) purge scan into per-datagram work
/// (panel F4, inv #10).
pub const PURGE_INTERVAL: Duration = Duration::from_secs(1);

/// Bind a UDP socket for a SAP transport with IPv6-first **dual-stack** semantics
/// (ADR-0042, panel S4), mirroring the webrtc media bind.
///
/// An IPv6 (`[::]`) bind sets `IPV6_V6ONLY=false` so it also accepts IPv4-mapped
/// peers, and every bind sets `SO_REUSEADDR` so multiple SAP listeners can share
/// the well-known port and a restart can re-bind promptly. tokio's
/// `UdpSocket::bind` sets **neither** — dual-stack is left to the host
/// `bindv6only` sysctl and `SO_REUSEADDR` is off — so the socket is built through
/// `socket2` (the crate stays `unsafe_code = forbid`; `SockRef`/`Socket`, never
/// raw FFI) and adopted as an async socket.
fn bind_dual_stack(local: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if local.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if local.is_ipv6() {
        // Dual-stack: accept IPv4-mapped peers on the same v6 socket (never pin to
        // v6-only, whatever the host `bindv6only` default).
        socket.set_only_v6(false)?;
    }
    // Share the well-known SAP port across listeners and re-bind promptly after a
    // restart (the webrtc media bind sets this too).
    socket.set_reuse_address(true)?;
    // tokio adopts the fd, which must be non-blocking.
    socket.set_nonblocking(true)?;
    socket.bind(&local.into())?;
    UdpSocket::from_std(socket.into())
}

/// A supervised SAP receive listener: a bound UDP socket folding discovered
/// announcements into a wait-free, bounded session table.
#[derive(Debug)]
pub struct SapListener {
    socket: Arc<UdpSocket>,
    table: Arc<SapSessionTable>,
    started: Instant,
    /// Datagrams admitted into the expensive parse+fold path per `accept_window`
    /// (panel F4).
    accept_burst: u32,
    /// The fixed window the `accept_burst` applies over.
    accept_window: Duration,
    /// The IPv6 multicast interface the receive-group join uses (panel F6);
    /// unspecified (OS default) unless set via `with_interface`.
    interface: MulticastInterface,
}

impl SapListener {
    /// Bind a SAP listener to `local` (IPv6-first: pass `[::]:9875` for the live
    /// receiver; an ephemeral loopback port for tests).
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    // `bind_dual_stack` is synchronous — the dual-stack / SO_REUSEADDR options must
    // be set BEFORE bind, so it cannot use tokio's async `UdpSocket::bind`; `bind`
    // stays `async` for API stability (every caller, and #103, awaits it, matching
    // the peer transports and the output `Aes67UdpSender::bind`).
    #[allow(clippy::unused_async)]
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        // Dual-stack, SO_REUSEADDR (panel S4): a `[::]` listener also accepts
        // IPv4-mapped SAP, not left to the host `bindv6only` default.
        let socket =
            bind_dual_stack(local).map_err(|e| Error::Ingest(format!("sap bind {local}: {e}")))?;
        Ok(Self {
            socket: Arc::new(socket),
            table: Arc::new(SapSessionTable::new()),
            started: Instant::now(),
            accept_burst: DEFAULT_ACCEPT_BURST,
            accept_window: DEFAULT_ACCEPT_WINDOW,
            interface: MulticastInterface::Unspecified,
        })
    }

    /// Set the IPv6 multicast interface the receive-group join uses (default
    /// [`MulticastInterface::Unspecified`] → the OS default). IPv6 SAP groups are
    /// link/site-local, so a real deployment supplies the intended interface
    /// rather than relying on index 0 (panel F6); IPv4 joins via `INADDR_ANY`.
    #[must_use]
    pub fn with_interface(mut self, interface: MulticastInterface) -> Self {
        self.interface = interface;
        self
    }

    /// Use a caller-provided (shared) session table instead of a fresh one, so
    /// the control plane can read the same inventory this listener writes.
    #[must_use]
    pub fn with_table(mut self, table: Arc<SapSessionTable>) -> Self {
        self.table = table;
        self
    }

    /// Override the fold-path rate limit (default [`DEFAULT_ACCEPT_BURST`] per
    /// [`DEFAULT_ACCEPT_WINDOW`]): at most `burst` datagrams per `window` enter
    /// the expensive parse+fold, the rest are dropped cheaply after the `recv`
    /// (panel F4). A spoofed-origin flood therefore cannot force the O(n) RCU
    /// clone at line rate and starve the control-plane runtime (inv #10).
    #[must_use]
    pub fn with_rate_limit(mut self, burst: u32, window: Duration) -> Self {
        self.accept_burst = burst;
        self.accept_window = window;
        self
    }

    /// A shared handle to the session table this listener folds into (read its
    /// wait-free [`inventory`](SapSessionTable::inventory) from anywhere).
    #[must_use]
    pub fn table(&self) -> Arc<SapSessionTable> {
        Arc::clone(&self.table)
    }

    /// The socket's local address.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket
            .local_addr()
            .map_err(|e| Error::Ingest(format!("sap local_addr: {e}")))
    }

    /// Join the SAP receive group set for this socket's address family (the live
    /// multicast discovery path). A dual-stack deployment runs a v4 and a v6
    /// listener; each joins only its own family's groups (a single socket cannot
    /// join the other family).
    ///
    /// The IPv6 SAP groups are link/site-local, so the join uses the configured
    /// [`interface`](Self::with_interface) index rather than a hardcoded `0`
    /// (panel F6); IPv4 joins via `INADDR_ANY`. **Live validation on a real IPv6
    /// multicast network is hardware-gated** (rule 26).
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if a membership cannot be joined.
    pub fn join_receive_groups(&self) -> Result<()> {
        let want_v6 = self.local_addr()?.is_ipv6();
        for group in receive_group_set() {
            match group {
                IpAddr::V4(v4) if !want_v6 => {
                    self.socket
                        .join_multicast_v4(v4, Ipv4Addr::UNSPECIFIED)
                        .map_err(|e| Error::Ingest(format!("sap join {v4}: {e}")))?;
                }
                IpAddr::V6(v6) if want_v6 => {
                    self.socket
                        .join_multicast_v6(&v6, self.interface.index())
                        .map_err(|e| Error::Ingest(format!("sap join {v6}: {e}")))?;
                }
                _ => {} // the other family — joined by that family's listener
            }
        }
        Ok(())
    }

    /// Fold one already-received datagram (`bytes`) into the table; returns whether
    /// it entered the expensive fold path (parsed **and** rate-admitted).
    ///
    /// The datagram is **parsed first** — a cheap, bounded structural check
    /// (version / length / non-zero hash, plus a size-capped `C=1` inflate): a
    /// malformed datagram is rejected here **without** spending rate budget, so a
    /// malformed flood cannot drain the shared fixed-window bucket and starve
    /// legitimate SAP (panel S3). The rate limiter then gates only the **expensive**
    /// O(n) RCU clone + publish, dropping a valid-but-flooding announce cheaply
    /// rather than forcing that work per datagram and starving the shared
    /// control-plane runtime (panel F4, inv #10). A malformed datagram is skipped,
    /// never faulted (rule 26: bad inputs are the purpose).
    fn fold_datagram(&self, bytes: &[u8], now: Duration, limiter: &mut SapRateLimiter) -> bool {
        // Parse BEFORE the rate gate: a malformed datagram fails here (cheap,
        // bounded) and consumes no budget, so a malformed flood can't starve legit
        // announces out of the shared bucket (panel S3).
        let Ok(pkt) = SapPacket::parse(bytes) else {
            return false;
        };
        // Only a structurally-valid announcement spends budget; the limiter bounds
        // the expensive O(n) RCU fold, not the cheap parse (panel F4, inv #10).
        if !limiter.allow(now) {
            return false;
        }
        self.table.observe(&pkt, now);
        true
    }

    /// Run the receive loop until the socket faults (then the supervisor restarts
    /// it), rate-gating each datagram into the wait-free table and purging stale
    /// sessions on a cadence. Never blocks a consumer or paces the engine
    /// (inv #1/#10).
    ///
    /// # Errors
    ///
    /// Returns `Ok(())` on a clean socket-fault exit; it does not surface the
    /// fault (the supervisor reconnects) but logs it.
    pub async fn run(self) -> Result<()> {
        let mut buf = vec![0u8; MAX_SAP_DATAGRAM];
        let mut limiter = SapRateLimiter::new(self.accept_burst, self.accept_window);
        // An INDEPENDENT purge timer, selected against the receive: expired
        // sessions are reaped on their own cadence even when announcements have
        // stopped and `recv_from` would otherwise park the loop forever (panel
        // F3). Consume the immediate first tick so the cadence is steady. Purge
        // stays off the per-datagram path, so a flood still cannot amplify the
        // O(n) scan into per-datagram work (panel F4).
        let mut purge_tick = tokio::time::interval(PURGE_INTERVAL);
        // Skip missed ticks: if the receive branch keeps the loop busy across
        // several purge periods, reap ONCE on return rather than bursting one
        // redundant O(n) scan per missed tick (panel I1). Purge is idempotent, so a
        // skipped tick loses nothing but the wasted scan.
        purge_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        purge_tick.tick().await;
        loop {
            tokio::select! {
                received = self.socket.recv_from(&mut buf) => {
                    let n = match received {
                        Ok((n, _peer)) => n,
                        Err(e) => {
                            tracing::debug!(error = %e, "sap listener socket ended; supervisor restarts");
                            return Ok(());
                        }
                    };
                    let now = self.started.elapsed();
                    if let Some(bytes) = buf.get(..n) {
                        // Rate-gated: a flood is dropped after the cheap recv,
                        // before the expensive fold (panel F4).
                        self.fold_datagram(bytes, now, &mut limiter);
                    }
                }
                _ = purge_tick.tick() => {
                    // Reap expired sessions regardless of datagram arrival (panel
                    // F3). Off the output data plane — never paces the engine
                    // (inv #1/#10).
                    self.table.purge(self.started.elapsed());
                }
            }
        }
    }
}

/// One session an announcer re-announces: its stable hash, originating source,
/// opaque SDP body, and the destination (the scope-selected SAP group on the SAP
/// port) to send to.
#[derive(Debug, Clone)]
pub struct AnnouncedSession {
    /// The stable non-zero message-id hash (see [`stable_hash`](super::announce::stable_hash)).
    pub hash: NonZeroU16,
    /// The announcement's originating source address.
    pub origin: IpAddr,
    /// The opaque SDP body to announce.
    pub sdp: Vec<u8>,
    /// The destination: the scope-selected SAP group on the SAP port.
    pub dest: SocketAddr,
}

/// A SAP announcer: a bound UDP socket that emits `T=0` announcements on a
/// jittered timer and `T=1` deletions on teardown.
#[derive(Debug)]
pub struct SapAnnouncer {
    socket: Arc<UdpSocket>,
    /// The IPv6 multicast egress interface (panel F9); unspecified (OS default)
    /// unless set via [`with_interface`](Self::with_interface).
    interface: MulticastInterface,
}

impl SapAnnouncer {
    /// Bind an announcer socket to `local`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    // `bind_dual_stack` is synchronous — the dual-stack / SO_REUSEADDR options must
    // be set BEFORE bind, so it cannot use tokio's async `UdpSocket::bind`; `bind`
    // stays `async` for API stability (every caller, and #103, awaits it, matching
    // the peer transports and the output `Aes67UdpSender::bind`).
    #[allow(clippy::unused_async)]
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        // Dual-stack, SO_REUSEADDR (panel S4), mirroring the listener bind.
        let socket = bind_dual_stack(local)
            .map_err(|e| Error::Ingest(format!("sap announcer bind {local}: {e}")))?;
        // SAP uses TTL/hop-limit 255 (scope is carried by the group address, not
        // the TTL). Best-effort: a v6-only socket has no v4 multicast TTL.
        if let Err(e) = socket.set_multicast_ttl_v4(SAP_TTL) {
            tracing::trace!(error = %e, "sap v4 multicast ttl unset (non-fatal, e.g. v6 socket)");
        }
        Ok(Self {
            socket: Arc::new(socket),
            interface: MulticastInterface::Unspecified,
        })
    }

    /// Set the IPv6 multicast **egress** interface for scoped announcements
    /// (default [`MulticastInterface::Unspecified`] → the OS default). IPv6 SAP
    /// groups are link/site-local, so a multi-homed real deployment supplies the
    /// intended interface rather than relying on index 0 (panel F9). Applied by
    /// [`configure_multicast_egress`](Self::configure_multicast_egress).
    #[must_use]
    pub fn with_interface(mut self, interface: MulticastInterface) -> Self {
        self.interface = interface;
        self
    }

    /// Apply the IPv6 multicast egress options to the bound socket: select the
    /// [`interface`](Self::with_interface) (`IPV6_MULTICAST_IF`) and set hop-limit
    /// [`SAP_TTL`] (`IPV6_MULTICAST_HOPS`). Call once before announcing on a
    /// scoped-multicast IPv6 dest; family-wise a no-op on a v4 socket (whose
    /// multicast TTL is set at [`bind`](Self::bind)).
    ///
    /// The egress interface index comes from config or the SDP scope and is wired
    /// by the pipeline (#103); until then it defaults to the OS default rather
    /// than a hardcoded `0`. **Live validation on a real IPv6 multicast network is
    /// hardware-gated** (rule 26).
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the local address cannot be read or the OS rejects the
    /// egress interface / hop-limit (e.g. a nonexistent interface index).
    pub fn configure_multicast_egress(&self) -> Result<()> {
        let local = self
            .socket
            .local_addr()
            .map_err(|e| Error::Ingest(format!("sap announcer local_addr: {e}")))?;
        if local.is_ipv6() {
            let sock = socket2::SockRef::from(self.socket.as_ref());
            sock.set_multicast_if_v6(self.interface.index())
                .map_err(|e| Error::Ingest(format!("sap set multicast egress if v6: {e}")))?;
            sock.set_multicast_hops_v6(SAP_TTL)
                .map_err(|e| Error::Ingest(format!("sap set multicast hops v6: {e}")))?;
        }
        Ok(())
    }

    /// Encode and send one SAP packet to `dest`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the send fails.
    pub async fn send_to(&self, packet: &SapPacket, dest: SocketAddr) -> Result<usize> {
        let bytes = packet.encode();
        self.socket
            .send_to(&bytes, dest)
            .await
            .map_err(|e| Error::Ingest(format!("sap send {dest}: {e}")))
    }

    /// Apply the configured IPv6 multicast egress, then announce every session
    /// once immediately and re-announce on the jittered [`AnnounceSchedule`]
    /// cadence forever (the supervisor aborts the task on shutdown and calls
    /// [`send_deletions`](Self::send_deletions)). An independent timer that never
    /// paces the output clock (inv #1). A per-send error is logged, not fatal — the
    /// next cycle retries. If applying the configured egress interface fails (a
    /// misconfigured index), the task ends with a warning rather than announcing on
    /// the wrong / OS-default egress.
    pub async fn run(self, sessions: Vec<AnnouncedSession>, schedule: AnnounceSchedule) {
        // Select the IPv6 multicast egress interface (IPV6_MULTICAST_IF + hop-limit)
        // before announcing (panel S2): F9 wired `configure_multicast_egress` but
        // nothing on the run path called it, so a configured interface was silently
        // ignored and egress fell to the OS default. Unspecified (the default until
        // #103 supplies the index) configures cleanly; a misconfigured interface
        // ends the task rather than announcing on the wrong egress — the supervisor
        // (#103) restarts or the operator fixes the config.
        if let Err(e) = self.configure_multicast_egress() {
            tracing::warn!(error = %e, "sap announcer egress config failed; not announcing");
            return;
        }
        let mut rng = seed(&sessions);
        loop {
            for s in &sessions {
                let pkt = announcement(s.hash, s.origin, s.sdp.clone());
                if let Err(e) = self.send_to(&pkt, s.dest).await {
                    tracing::debug!(error = %e, dest = %s.dest, "sap announce send failed");
                }
            }
            rng = xorshift(rng);
            tokio::time::sleep(schedule.next_delay(rng)).await;
        }
    }

    /// Send a courtesy `T=1` deletion for each session (best-effort teardown).
    pub async fn send_deletions(&self, sessions: &[AnnouncedSession]) {
        for s in sessions {
            let pkt = deletion(s.hash, s.origin, s.sdp.clone());
            if let Err(e) = self.send_to(&pkt, s.dest).await {
                tracing::debug!(error = %e, dest = %s.dest, "sap deletion send failed");
            }
        }
    }
}

/// A per-run jitter seed that de-correlates announcers across hosts/sessions:
/// wall-clock nanoseconds mixed with the first session's hash. Non-cryptographic
/// — only spreads the ±1/3 announce jitter. Never zero (xorshift needs non-zero
/// state).
fn seed(sessions: &[AnnouncedSession]) -> u64 {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut s = u64::try_from(base & u128::from(u64::MAX)).unwrap_or(0x9E37_79B9_7F4A_7C15);
    if let Some(first) = sessions.first() {
        s ^= u64::from(first.hash.get());
    }
    s | 1
}

/// A tiny non-cryptographic xorshift64 step for the announce jitter sample.
fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use crate::sap::packet::{SapMessageType, SapPacket};
    use crate::sap::ratelimit::SapRateLimiter;
    use crate::sap::transport::SapListener;
    use std::net::{IpAddr, Ipv6Addr, SocketAddr};
    use std::num::NonZeroU16;
    use std::time::Duration;

    fn loopback6() -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
    }

    /// A minimal well-formed announcement datagram carrying `hash`.
    fn announce_datagram(hash: u16) -> Vec<u8> {
        SapPacket {
            message_type: SapMessageType::Announcement,
            msg_id_hash: NonZeroU16::new(hash).unwrap(),
            origin: IpAddr::V6(Ipv6Addr::LOCALHOST),
            payload_type: None,
            payload: b"v=0\r\ns=legit\r\n".to_vec(),
        }
        .encode()
    }

    #[tokio::test]
    async fn a_malformed_flood_does_not_starve_legit_sap_of_rate_budget() {
        // S3 (#157): the rate limiter must gate the EXPENSIVE fold AFTER a cheap
        // structural parse, not before. A malformed datagram fails parse cheaply and
        // must NOT consume budget — otherwise a malformed flood drains the shared
        // fixed-window bucket and a legit announce in the same window is dropped by
        // the limiter even though it would parse and fold fine.
        let listener = SapListener::bind(loopback6()).await.unwrap();
        let table = listener.table();
        // A budget of ONE fold per (long) window: if a malformed datagram spends it,
        // the legit announce that follows in the same window is starved.
        let mut limiter = SapRateLimiter::new(1, Duration::from_secs(3600));
        let now = Duration::ZERO;

        // A malformed flood FIRST (version 0 -> BadVersion; a single byte).
        for _ in 0..64 {
            let admitted = listener.fold_datagram(&[0x00_u8], now, &mut limiter);
            assert!(
                !admitted,
                "a malformed datagram must not enter the fold nor spend rate budget (S3)"
            );
        }
        // Then one legit announcement, within the SAME window.
        let admitted = listener.fold_datagram(&announce_datagram(0x1234), now, &mut limiter);

        assert!(
            admitted,
            "the legit announce must enter the fold — a malformed flood must not burn \
             its budget (S3)"
        );
        assert_eq!(
            table.len(),
            1,
            "the legit session is observed despite a preceding malformed flood (S3)"
        );
    }

    #[tokio::test]
    async fn listener_binds_ipv6_unspecified_as_dual_stack() {
        // S4 (#158): binding [::] must be dual-stack (IPV6_V6ONLY=false) so it also
        // accepts IPv4-mapped SAP, and must set SO_REUSEADDR — mirroring the webrtc
        // media bind (ADR-0042 IPv6-first). tokio's UdpSocket::bind sets NEITHER, so
        // dual-stack is left to the host `bindv6only` sysctl and SO_REUSEADDR is off;
        // both are now set explicitly via socket2 for a deterministic bind.
        let listener = SapListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
            .await
            .unwrap();
        let sock = socket2::SockRef::from(listener.socket.as_ref());
        assert!(
            !sock.only_v6().unwrap(),
            "the [::] SAP listener must bind dual-stack (only_v6 == false) (S4)"
        );
        assert!(
            sock.reuse_address().unwrap(),
            "the [::] SAP listener must set SO_REUSEADDR like the webrtc media bind (S4)"
        );
    }
}
