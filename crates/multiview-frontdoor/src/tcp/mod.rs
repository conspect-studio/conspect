//! The TCP door, slice 4 (issue #290; design note sections 4, 8 and 13).
//!
//! One TCP listener carries TLS (the site over HTTP/1.1, and later the
//! TLS-ALPN-01 responder and TURN-TLS) and ICE-TCP (RFC 6544 passive
//! candidates), told apart by the first byte. In `behind_proxy` mode the
//! listener sits behind a reverse proxy and requires a PROXY v2 header from a
//! trusted source, failing closed. A separate plaintext listener answers the
//! port-80 redirect.
//!
//! Every step before authentication is bounded (invariant #10): a global and a
//! per-source connection cap enforced at accept before any byte is read, a 5 s
//! first-byte deadline from accept, a 10 s TLS handshake deadline from the
//! first byte, a 16 KiB `ClientHello` cap, a 1535-byte first ICE-TCP frame and
//! the ICE-TCP probation. Everything runs on the front door's own runtime; the
//! only thing shared with a consumer is the bounded ICE-TCP lane.

pub mod classify;
pub mod ice_tcp;
pub mod proxy_v2;
pub mod redirect;
pub mod tls;

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use crate::classify::SourceKey;
use crate::Stats;

pub use classify::{classify_first_bytes, TcpClass};
pub use ice_tcp::{IceTcpFrame, IceTcpLane, IceTcpSender, SendOutcome, UsernameCheck};
pub use tls::{DoorStream, TlsRouting};

/// The handshaken TLS stream a [`SiteService`] serves.
pub type DoorTlsStream = tokio_rustls::server::TlsStream<DoorStream>;

/// A boxed, sendable future (the [`SiteService`] return type).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Default global TCP connection cap (the ADR-W031 floor).
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;
/// Default per-source cap (IPv4 address or IPv6 /64).
pub const DEFAULT_MAX_PER_SOURCE: usize = 256;
/// Default deadline for the first byte (and, behind a proxy, the PROXY
/// header), from accept.
pub const DEFAULT_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(5);
/// Default TLS handshake deadline, from the first byte.
pub const DEFAULT_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default ICE-TCP probation (an outbound frame must be queued by then).
pub const DEFAULT_ICE_PROBATION: Duration = Duration::from_secs(5);
/// Default ICE-TCP idle timeout (no inbound frame for this long closes).
pub const DEFAULT_ICE_IDLE: Duration = Duration::from_secs(30);
/// Default ICE-TCP write stall bound (a write, or a continuously overflowing
/// queue, for this long closes the connection as stuck).
pub const DEFAULT_ICE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The listen backlog.
pub const BACKLOG: i32 = 1024;
/// Accept-error backoff (EMFILE and friends): never busy-spin, never exit.
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// PROXY v2 sources trusted in `behind_proxy` mode: address prefixes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    prefixes: Vec<(IpAddr, u8)>,
}

impl TrustedProxies {
    /// Build from `(address, prefix length)` pairs. IPv4-mapped addresses are
    /// treated as IPv4.
    #[must_use]
    pub fn new(prefixes: impl IntoIterator<Item = (IpAddr, u8)>) -> Self {
        Self {
            prefixes: prefixes.into_iter().map(|(ip, p)| (unmap(ip), p)).collect(),
        }
    }

    /// Whether `ip` is a trusted proxy.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = unmap(ip);
        self.prefixes.iter().any(|&(net, prefix)| match (net, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => prefix_eq(
                u128::from(u32::from(n)) << 96,
                u128::from(u32::from(i)) << 96,
                prefix,
            ),
            (IpAddr::V6(n), IpAddr::V6(i)) => prefix_eq(u128::from(n), u128::from(i), prefix),
            _ => false,
        })
    }
}

fn prefix_eq(a: u128, b: u128, prefix: u8) -> bool {
    let shift = 128u32.saturating_sub(u32::from(prefix));
    a.checked_shr(shift).unwrap_or(0) == b.checked_shr(shift).unwrap_or(0)
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// The TCP door's settings.
#[derive(Debug, Clone)]
pub struct TcpDoorConfig {
    /// `[bind]:tcp_port` (direct) or `proxy_listen` (behind a proxy). Port 0
    /// picks an ephemeral port (tests). Never `0.0.0.0`.
    pub listen: SocketAddr,
    /// `Some`: behind a proxy, PROXY v2 required from these sources.
    pub proxy: Option<TrustedProxies>,
    /// Global connection cap.
    pub max_connections: usize,
    /// Per-source cap (IPv4 address / IPv6 /64; the PROXY client behind a
    /// proxy).
    pub max_per_source: usize,
    /// Deadline for the first byte (and the PROXY header), from accept.
    pub first_byte_timeout: Duration,
    /// TLS handshake deadline (`ClientHello` read and handshake), from the
    /// first byte.
    pub tls_handshake_timeout: Duration,
    /// Accept ICE-TCP.
    pub ice_tcp: bool,
    /// ICE-TCP probation deadline, from accept.
    pub ice_probation: Duration,
    /// ICE-TCP idle timeout.
    pub ice_idle: Duration,
    /// ICE-TCP write stall bound.
    pub ice_write_timeout: Duration,
    /// `turn.<public_host>`: reserved for TURN-TLS (slice 8), closed for now.
    pub turn_sni: Option<String>,
}

impl Default for TcpDoorConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 443),
            proxy: None,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_per_source: DEFAULT_MAX_PER_SOURCE,
            first_byte_timeout: DEFAULT_FIRST_BYTE_TIMEOUT,
            tls_handshake_timeout: DEFAULT_TLS_HANDSHAKE_TIMEOUT,
            ice_tcp: true,
            ice_probation: DEFAULT_ICE_PROBATION,
            ice_idle: DEFAULT_ICE_IDLE,
            ice_write_timeout: DEFAULT_ICE_WRITE_TIMEOUT,
            turn_sni: None,
        }
    }
}

/// The port-80 redirect listener's settings.
#[derive(Debug, Clone)]
pub struct RedirectConfig {
    /// `[bind]:http_redirect_port`.
    pub listen: SocketAddr,
    /// The configured host the redirect points at (never the `Host` header).
    pub public_host: String,
    /// The HTTPS port the redirect points at.
    pub https_port: u16,
    /// Global cap.
    pub max_connections: usize,
    /// Per-source cap.
    pub max_per_source: usize,
    /// Total deadline per connection.
    pub deadline: Duration,
}

impl RedirectConfig {
    /// The defaults (caps 256 / 16, 5 s) for a host and port.
    #[must_use]
    pub fn new(listen: SocketAddr, public_host: String, https_port: u16) -> Self {
        Self {
            listen,
            public_host,
            https_port,
            max_connections: 256,
            max_per_source: 16,
            deadline: Duration::from_secs(5),
        }
    }
}

/// Serves one accepted, handshaken TLS connection (the site).
pub trait SiteService: Send + Sync + 'static {
    /// Serve `io` to its end. Runs as (part of) the per-connection task on the
    /// front-door runtime. `peer` is the real client (the PROXY v2 source
    /// behind a proxy). `guard` is the admission slot: keep it until the
    /// connection really ends (including after an HTTP upgrade).
    fn serve(&self, io: DoorTlsStream, peer: SocketAddr, guard: ConnGuard) -> BoxFuture<()>;
}

/// Which gauge an [`Admission`] maintains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gauge {
    Door,
    Redirect,
}

/// Accept-time admission: a global cap and a per-source cap. Bounded by the
/// number of concurrent connections (a per-source entry is removed at zero).
#[derive(Debug)]
pub(crate) struct Admission {
    open: AtomicUsize,
    max: usize,
    max_per_source: usize,
    per_source: Mutex<HashMap<SourceKey, usize>>,
    stats: Arc<Stats>,
    gauge: Gauge,
}

impl Admission {
    fn new(max: usize, max_per_source: usize, stats: Arc<Stats>, gauge: Gauge) -> Arc<Self> {
        Arc::new(Self {
            open: AtomicUsize::new(0),
            max: max.max(1),
            max_per_source: max_per_source.max(1),
            per_source: Mutex::new(HashMap::new()),
            stats,
            gauge,
        })
    }

    fn publish(&self, open: usize) {
        let open = u64::try_from(open).unwrap_or(u64::MAX);
        match self.gauge {
            Gauge::Door => self
                .stats
                .tcp_open_connections
                .store(open, Ordering::Relaxed),
            Gauge::Redirect => self
                .stats
                .redirect_open_connections
                .store(open, Ordering::Relaxed),
        }
    }

    /// A global slot, or `None` at the cap.
    fn try_admit_global(self: &Arc<Self>) -> Option<ConnGuard> {
        let mut open = self.open.load(Ordering::Relaxed);
        loop {
            if open >= self.max {
                return None;
            }
            match self.open.compare_exchange_weak(
                open,
                open + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => open = now,
            }
        }
        self.publish(open + 1);
        Some(ConnGuard {
            admission: Arc::clone(self),
            source: None,
        })
    }

    fn per_source(&self) -> std::sync::MutexGuard<'_, HashMap<SourceKey, usize>> {
        self.per_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One admitted connection's slot (global, and per source once known).
/// Released on drop.
#[derive(Debug)]
pub struct ConnGuard {
    admission: Arc<Admission>,
    source: Option<SourceKey>,
}

impl ConnGuard {
    /// Take the per-source slot for `peer`; `false` at the per-source cap.
    fn admit_source(&mut self, peer: SocketAddr) -> bool {
        let key = SourceKey::of(peer);
        let mut map = self.admission.per_source();
        let count = map.get(&key).copied().unwrap_or(0);
        if count >= self.admission.max_per_source {
            return false;
        }
        map.insert(key, count + 1);
        self.source = Some(key);
        true
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(key) = self.source.take() {
            let mut map = self.admission.per_source();
            if let Some(count) = map.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    map.remove(&key);
                }
            }
        }
        let before = self.admission.open.fetch_sub(1, Ordering::AcqRel);
        self.admission.publish(before.saturating_sub(1));
    }
}

/// The swappable parts of the door (installed after the control plane is
/// built).
#[derive(Default)]
pub(crate) struct DoorRouting {
    pub(crate) tls: RwLock<TlsRouting>,
    pub(crate) site: RwLock<Option<Arc<dyn SiteService>>>,
}

impl DoorRouting {
    fn snapshot(&self) -> (TlsRouting, Option<Arc<dyn SiteService>>) {
        let tls = self
            .tls
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let site = self
            .site
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (tls, site)
    }
}

/// Bind a TCP listener in the current runtime: dual-stack for `[::]`, never
/// `0.0.0.0`, backlog [`BACKLOG`].
pub(crate) fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    if addr.ip() == IpAddr::V4(Ipv4Addr::UNSPECIFIED) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the front door binds [::] dual-stack, never 0.0.0.0",
        ));
    }
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() && addr.ip().is_unspecified() {
        socket.set_only_v6(false)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_tcp_nodelay(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(BACKLOG)?;
    TcpListener::from_std(socket.into())
}

/// The running door's shared state.
pub(crate) struct TcpDoorState {
    pub(crate) config: TcpDoorConfig,
    pub(crate) admission: Arc<Admission>,
    pub(crate) routing: Arc<DoorRouting>,
    pub(crate) ice: Arc<ice_tcp::IceShared>,
    pub(crate) stats: Arc<Stats>,
}

impl TcpDoorState {
    pub(crate) fn new(
        config: TcpDoorConfig,
        tls: TlsRouting,
        site: Option<Arc<dyn SiteService>>,
        stats: Arc<Stats>,
    ) -> Arc<Self> {
        let routing = Arc::new(DoorRouting {
            tls: RwLock::new(tls),
            site: RwLock::new(site),
        });
        Arc::new(Self {
            admission: Admission::new(
                config.max_connections,
                config.max_per_source,
                Arc::clone(&stats),
                Gauge::Door,
            ),
            ice: ice_tcp::IceShared::new(config.max_connections, Arc::clone(&stats)),
            config,
            routing,
            stats,
        })
    }
}

/// The accept loop of the TCP door.
pub(crate) async fn accept_loop(listener: TcpListener, state: Arc<TcpDoorState>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            Stats::bump(&state.stats.tcp_accept_errors);
            tokio::time::sleep(ACCEPT_BACKOFF).await;
            continue;
        };
        let accepted_at = tokio::time::Instant::now();
        // Admission before reading any byte.
        let Some(mut guard) = state.admission.try_admit_global() else {
            Stats::bump(&state.stats.tcp_refused_global);
            drop(stream);
            continue;
        };
        if state.config.proxy.is_none() && !guard.admit_source(peer) {
            Stats::bump(&state.stats.tcp_refused_per_source);
            drop(stream);
            continue;
        }
        if let Some(trusted) = &state.config.proxy {
            if !trusted.contains(peer.ip()) {
                Stats::bump(&state.stats.tcp_proxy_rejected);
                drop(stream);
                continue;
            }
        }
        Stats::bump(&state.stats.tcp_accepted);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            serve_connection(state, stream, peer, guard, accepted_at).await;
        });
    }
}

async fn serve_connection(
    state: Arc<TcpDoorState>,
    mut stream: TcpStream,
    tcp_peer: SocketAddr,
    mut guard: ConnGuard,
    accepted_at: tokio::time::Instant,
) {
    let counters = &state.stats;
    let deadline = accepted_at + state.config.first_byte_timeout;
    let peer = if state.config.proxy.is_some() {
        let Ok(Ok(client)) = tokio::time::timeout_at(deadline, proxy_v2::read(&mut stream)).await
        else {
            Stats::bump(&counters.tcp_proxy_rejected);
            return;
        };
        if !guard.admit_source(client) {
            Stats::bump(&counters.tcp_refused_per_source);
            return;
        }
        client
    } else {
        tcp_peer
    };
    // First byte, peeked: nothing is consumed before the decision.
    let mut first = [0u8; 1];
    match tokio::time::timeout_at(deadline, stream.peek(&mut first)).await {
        Ok(Ok(1)) => {}
        Ok(_) => {
            Stats::bump(&counters.tcp_classified_close);
            return;
        }
        Err(_) => {
            Stats::bump(&counters.tcp_first_byte_timeout);
            return;
        }
    }
    let first_byte_at = tokio::time::Instant::now();
    let class = match classify_first_bytes(&first) {
        TcpClass::NeedMore if state.config.ice_tcp => {
            // 0x00-0x05 is never TLS: consuming the length prefix is safe.
            let mut prefix = [0u8; 2];
            match tokio::time::timeout_at(deadline, stream.read_exact(&mut prefix)).await {
                Ok(Ok(_)) => classify_first_bytes(&prefix),
                Ok(Err(_)) => TcpClass::Close,
                Err(_) => {
                    Stats::bump(&counters.tcp_first_byte_timeout);
                    return;
                }
            }
        }
        TcpClass::NeedMore => TcpClass::Close,
        other => other,
    };
    match class {
        TcpClass::Tls => {
            serve_tls(&state, stream, peer, guard, first_byte_at).await;
        }
        TcpClass::IceTcp { len } => {
            let timing = ice_tcp::IceTcpTiming {
                probation: state.config.ice_probation,
                idle: state.config.ice_idle,
                write_timeout: state.config.ice_write_timeout,
            };
            ice_tcp::serve(
                Arc::clone(&state.ice),
                stream,
                peer,
                len,
                deadline,
                accepted_at,
                timing,
                guard,
            )
            .await;
        }
        TcpClass::NeedMore | TcpClass::Close => {
            Stats::bump(&counters.tcp_classified_close);
        }
    }
}

async fn serve_tls(
    state: &TcpDoorState,
    stream: TcpStream,
    peer: SocketAddr,
    guard: ConnGuard,
    first_byte_at: tokio::time::Instant,
) {
    let counters = &state.stats;
    let deadline = first_byte_at + state.config.tls_handshake_timeout;
    let io = DoorStream::capped(stream, tls::MAX_CLIENT_HELLO);
    let acceptor = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), io);
    let mut start = match tokio::time::timeout_at(deadline, acceptor).await {
        Ok(Ok(start)) => start,
        Ok(Err(_)) => {
            Stats::bump(&counters.tcp_tls_failed);
            return;
        }
        Err(_) => {
            Stats::bump(&counters.tcp_tls_handshake_timeout);
            return;
        }
    };
    let (routing, site) = state.routing.snapshot();
    let route = tls::route_hello(&start.client_hello(), state.config.turn_sni.as_deref());
    let config = match route {
        tls::HelloRoute::TurnReserved => {
            Stats::bump(&counters.tcp_turn_reserved);
            return;
        }
        tls::HelloRoute::Acme => routing.acme,
        tls::HelloRoute::Site => {
            if site.is_none() {
                Stats::bump(&counters.tcp_tls_failed);
                return;
            }
            routing.site
        }
    };
    let Some(config) = config else {
        Stats::bump(&counters.tcp_tls_failed);
        return;
    };
    start.io.lift_cap();
    let tls_stream = match tokio::time::timeout_at(deadline, start.into_stream(config)).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            Stats::bump(&counters.tcp_tls_failed);
            return;
        }
        Err(_) => {
            Stats::bump(&counters.tcp_tls_handshake_timeout);
            return;
        }
    };
    if route == tls::HelloRoute::Acme {
        // The challenge handshake is the whole exchange (RFC 8737 section 3).
        Stats::bump(&counters.tcp_tls_acme);
        let mut tls_stream = tls_stream;
        let _ = tokio::time::timeout(Duration::from_secs(1), tls_stream.shutdown()).await;
        drop(guard);
    } else if let Some(site) = site {
        Stats::bump(&counters.tcp_tls_site);
        site.serve(tls_stream, peer, guard).await;
    }
}

/// The redirect listener's state.
pub(crate) struct RedirectState {
    pub(crate) config: RedirectConfig,
    admission: Arc<Admission>,
    stats: Arc<Stats>,
}

impl RedirectState {
    pub(crate) fn new(config: RedirectConfig, stats: Arc<Stats>) -> Arc<Self> {
        Arc::new(Self {
            admission: Admission::new(
                config.max_connections,
                config.max_per_source,
                Arc::clone(&stats),
                Gauge::Redirect,
            ),
            config,
            stats,
        })
    }
}

/// The accept loop of the redirect listener.
pub(crate) async fn redirect_loop(listener: TcpListener, state: Arc<RedirectState>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            Stats::bump(&state.stats.tcp_accept_errors);
            tokio::time::sleep(ACCEPT_BACKOFF).await;
            continue;
        };
        let Some(mut guard) = state.admission.try_admit_global() else {
            Stats::bump(&state.stats.redirect_refused);
            continue;
        };
        if !guard.admit_source(peer) {
            Stats::bump(&state.stats.redirect_refused);
            continue;
        }
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _guard = guard;
            let deadline = state.config.deadline;
            let served = tokio::time::timeout(deadline, redirect_one(&state.config, stream)).await;
            if matches!(served, Ok(Ok(()))) {
                Stats::bump(&state.stats.redirect_served);
            }
        });
    }
}

async fn redirect_one(config: &RedirectConfig, mut stream: TcpStream) -> std::io::Result<()> {
    let mut head = vec![0u8; redirect::MAX_REQUEST_HEAD];
    let mut filled = 0usize;
    while let Some(slot) = head.get_mut(filled..) {
        if slot.is_empty() {
            break;
        }
        let n = stream.read(slot).await?;
        if n == 0 {
            break;
        }
        let seen_from = filled.saturating_sub(1);
        filled += n;
        if head
            .get(seen_from..filled)
            .is_some_and(|w| w.windows(2).any(|p| p == b"\r\n"))
        {
            break;
        }
    }
    let request = head.get(..filled).unwrap_or_default();
    let response = redirect::redirect_response(&config.public_host, config.https_port, request);
    stream.write_all(&response).await?;
    stream.shutdown().await
}
