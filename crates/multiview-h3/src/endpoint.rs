//! The noq endpoint on a caller-provided socket, its accept loop and caps.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

use crate::config::H3Limits;
use crate::counters::Counters;
use crate::server::serve_connection;
use crate::webtransport::unpoison;
use crate::{Error, Site, WtHandler};

/// The only ALPN the endpoint offers.
pub const ALPN_H3: &[u8] = b"h3";

/// Called `(remote, true)` once a handshake completes and `(remote, false)`
/// when that connection ends (also on abort).
pub type PeersHook = Arc<dyn Fn(SocketAddr, bool) + Send + Sync>;

/// Returns `true` when a QUIC source address must be refused before any cap
/// check runs (design note §8: a forwarder hop that lost the client's real
/// address must fail closed, the same way the TCP door fails closed on a
/// missing/untrusted PROXY v2 header).
pub type SourceFilter = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;

/// Everything [`H3Endpoint::start`] needs.
pub struct EndpointParts {
    /// The socket (the front door's QUIC lane, or a plain UDP socket).
    pub socket: Box<dyn noq::AsyncUdpSocket>,
    /// The endpoint config (the front door's, with `grease_quic_bit` off).
    pub endpoint: noq::EndpointConfig,
    /// `(max_incoming, incoming_buffer_size_total)` for noq.
    pub incoming: (usize, u64),
    /// The caller's TLS config; must enable TLS 1.3. ALPN, 0-RTT and the key
    /// log are overridden (see [`server_tls`]).
    pub tls: Arc<rustls::ServerConfig>,
    /// The runtime the endpoint and every connection task run on.
    pub runtime: tokio::runtime::Handle,
    /// Site requests.
    pub site: Arc<dyn Site>,
    /// WebTransport under the prefix, if any.
    pub wt: Option<Arc<dyn WtHandler>>,
    /// Limits.
    pub limits: H3Limits,
    /// Established-peer hook.
    pub peers: Option<PeersHook>,
    /// Optional per-source refusal, checked before any cap in the accept
    /// loop. `None` admits every source to the cap checks (direct mode, or
    /// `behind_proxy` with a source-preserving forward).
    pub refuse_source: Option<SourceFilter>,
    /// QUIC Retry policy (RFC 9000 §8.1.2; `network.quic.retry_above_handshakes`,
    /// design note "Slice 3 as built, part B"): once this many handshakes
    /// are in flight, an Initial from an unvalidated address is answered
    /// with a stateless Retry instead of handshake state. `Some(0)` retries
    /// every unvalidated Initial; `None` never retries.
    pub retry_above_handshakes: Option<usize>,
    /// Bumped once per Retry sent (the front door's metrics read it).
    pub retry_counter: Option<Arc<AtomicU64>>,
}

/// The TLS config the endpoint actually uses: a clone of `tls` with ALPN
/// exactly `h3`, 0-RTT off (`max_early_data_size = 0`; design note section
/// 5: every API mutation is non-idempotent) and no key log.
///
/// # Errors
///
/// [`Error::Config`] when `tls` does not enable TLS 1.3 (QUIC requires it).
pub fn server_tls(tls: &rustls::ServerConfig) -> Result<rustls::ServerConfig, Error> {
    let mut tls = tls.clone();
    tls.alpn_protocols = vec![ALPN_H3.to_vec()];
    tls.max_early_data_size = 0;
    tls.send_half_rtt_data = false;
    tls.key_log = Arc::new(rustls::NoKeyLog);
    // Probe exactly the check noq's session start performs (and unwraps):
    // a QUIC server connection needs TLS 1.3 enabled with a QUIC-capable
    // suite. Rejecting here turns a later panic into a config error.
    let tls = Arc::new(tls);
    if rustls::quic::ServerConnection::new(Arc::clone(&tls), rustls::quic::Version::V1, Vec::new())
        .is_err()
    {
        return Err(Error::Config(
            "the TLS config does not enable TLS 1.3 for QUIC",
        ));
    }
    Ok(Arc::unwrap_or_clone(tls))
}

/// The per-source key: the IPv4 address, or the IPv6 /64 (an IPv4-mapped
/// IPv6 address counts as its IPv4 address).
#[must_use]
pub fn source_key(addr: SocketAddr) -> IpAddr {
    match addr.ip() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut segments = v6.segments();
            for segment in segments.iter_mut().skip(4) {
                *segment = 0;
            }
            IpAddr::V6(std::net::Ipv6Addr::from(segments))
        }
    }
}

/// Global and per-source connection accounting. Bounded: one entry per
/// source with a live connection, and live connections are capped.
#[derive(Debug)]
struct Caps {
    total: usize,
    per_source: HashMap<IpAddr, usize>,
    max_total: usize,
    max_per_source: usize,
}

impl Caps {
    /// True when a connection from `remote` would be over a cap now (a
    /// cheap pre-check before the handshake; nothing is reserved).
    fn full(caps: &Arc<Mutex<Self>>, remote: SocketAddr) -> bool {
        let c = unpoison(caps.lock());
        c.total >= c.max_total
            || c.per_source.get(&source_key(remote)).copied().unwrap_or(0) >= c.max_per_source
    }

    fn try_take(caps: &Arc<Mutex<Self>>, remote: SocketAddr) -> Option<CapGuard> {
        let key = source_key(remote);
        let mut c = unpoison(caps.lock());
        if c.total >= c.max_total {
            return None;
        }
        let max_per_source = c.max_per_source;
        let entry = c.per_source.entry(key).or_insert(0);
        if *entry >= max_per_source {
            if *entry == 0 {
                c.per_source.remove(&key);
            }
            return None;
        }
        *entry += 1;
        c.total += 1;
        Some(CapGuard {
            caps: Arc::clone(caps),
            key,
        })
    }
}

/// Releases a connection's cap slot on drop (also on task abort).
struct CapGuard {
    caps: Arc<Mutex<Caps>>,
    key: IpAddr,
}

impl Drop for CapGuard {
    fn drop(&mut self) {
        let mut c = unpoison(self.caps.lock());
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.per_source.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.per_source.remove(&self.key);
            }
        }
    }
}

/// Calls the peers hook with `false` on drop (also on task abort).
struct PeerGuard {
    hook: PeersHook,
    remote: SocketAddr,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        (self.hook)(self.remote, false);
    }
}

/// A running HTTP/3 endpoint.
pub struct H3Endpoint {
    endpoint: noq::Endpoint,
    accept_task: JoinHandle<()>,
    counters: Arc<Counters>,
    shutdown_timeout: std::time::Duration,
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for H3Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Endpoint")
            .field("local", &self.endpoint.local_addr().ok())
            .finish_non_exhaustive()
    }
}

/// The noq server config: TLS from [`server_tls`], the transport limits and
/// the incoming caps.
///
/// # Errors
///
/// TLS without TLS 1.3, no QUIC initial cipher suite, or a bad limit.
pub fn server_config(
    tls: &rustls::ServerConfig,
    limits: &H3Limits,
    incoming: (usize, u64),
) -> Result<noq::ServerConfig, Error> {
    let tls = server_tls(tls)?;
    let crypto = noq::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls))
        .map_err(|_| Error::Config("the TLS config has no QUIC initial cipher suite"))?;
    let mut config = noq::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(limits.quic.transport_config()?));
    config
        .max_incoming(incoming.0)
        .incoming_buffer_size_total(incoming.1);
    Ok(config)
}

impl H3Endpoint {
    /// Builds the endpoint inside `parts.runtime` (noq spawns its driver and
    /// connection tasks on the runtime current at construction) and starts
    /// the accept loop there.
    ///
    /// # Errors
    ///
    /// A bad TLS config or limit, or noq failing to create the endpoint.
    pub async fn start(parts: EndpointParts) -> Result<Self, Error> {
        let EndpointParts {
            socket,
            endpoint,
            incoming,
            tls,
            runtime,
            site,
            wt,
            limits,
            peers,
            refuse_source,
            retry_above_handshakes,
            retry_counter,
        } = parts;
        let server = server_config(&tls, &limits, incoming)?;
        let counters = Arc::new(Counters::default());
        let shutdown_timeout = limits.shutdown_timeout;
        let loop_counters = Arc::clone(&counters);
        let (endpoint, accept_task) = runtime
            .spawn(async move {
                let noq_runtime: Arc<dyn noq::Runtime> = Arc::new(noq::TokioRuntime);
                let endpoint = noq::Endpoint::new_with_abstract_socket(
                    endpoint,
                    Some(server),
                    socket,
                    noq_runtime,
                )
                .map_err(Error::Io)?;
                let accept = tokio::runtime::Handle::current().spawn(accept_loop(
                    endpoint.clone(),
                    site,
                    wt,
                    limits,
                    peers,
                    Admission {
                        refuse_source,
                        retry: Retry {
                            above_handshakes: retry_above_handshakes,
                            counter: retry_counter,
                        },
                    },
                    loop_counters,
                ));
                Ok::<_, Error>((endpoint, accept))
            })
            .await
            .map_err(|_| Error::Config("endpoint construction task failed"))??;
        Ok(Self {
            endpoint,
            accept_task,
            counters,
            shutdown_timeout,
            runtime,
        })
    }

    /// The local address of the socket.
    ///
    /// # Errors
    ///
    /// The socket has no address.
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        self.endpoint.local_addr().map_err(Error::Io)
    }

    /// The endpoint's counters.
    #[must_use]
    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    /// The underlying noq endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &noq::Endpoint {
        &self.endpoint
    }

    /// Closes every connection and the endpoint, then waits (bounded by
    /// `H3Limits::shutdown_timeout`) for connections to send their close.
    pub async fn shutdown(self) {
        self.accept_task.abort();
        self.endpoint
            .close(noq::VarInt::from_u32(0x100), b"shutdown");
        let endpoint = self.endpoint.clone();
        let wait = self
            .runtime
            .spawn(async move { endpoint.wait_all_draining().await });
        let _ = tokio::time::timeout(self.shutdown_timeout, wait).await;
    }
}

/// Accounting of handshakes in progress (`H3Limits::max_handshakes`, and
/// per source). Separate from the connection caps: an incomplete handshake
/// never holds a connection slot.
#[derive(Debug)]
struct Handshakes(Arc<Mutex<Caps>>);

impl Handshakes {
    fn new(max_total: usize, max_per_source: usize) -> Self {
        Self(Arc::new(Mutex::new(Caps {
            total: 0,
            per_source: HashMap::new(),
            max_total,
            max_per_source,
        })))
    }

    /// A handshake slot, released on drop (also on task abort).
    fn try_take(&self, remote: SocketAddr) -> Option<CapGuard> {
        Caps::try_take(&self.0, remote)
    }

    /// Handshakes in flight now (slots taken and not yet released).
    fn in_flight(&self) -> usize {
        unpoison(self.0.lock()).total
    }
}

/// The accept loop's pre-handshake admission policy: the source filter and
/// the Retry policy, both applied before any handshake state is created.
struct Admission {
    refuse_source: Option<SourceFilter>,
    retry: Retry,
}

/// The accept loop's Retry policy ([`EndpointParts::retry_above_handshakes`]).
struct Retry {
    above_handshakes: Option<usize>,
    counter: Option<Arc<AtomicU64>>,
}

impl Retry {
    /// The same semantics as the vendored `MoQ` server's
    /// `moq_tokio::noq::Server::maybe_retry`: with `handshakes_in_flight`
    /// at or over the threshold, an `Incoming` whose address is not yet
    /// validated and that may still be retried is answered with a Retry
    /// (counted) and consumed. A `retry()` that fails is ignored instead;
    /// either way `None` is returned and the caller never accepts it.
    /// Otherwise `incoming` is returned unchanged.
    fn apply(&self, incoming: noq::Incoming, handshakes_in_flight: usize) -> Option<noq::Incoming> {
        let Some(threshold) = self.above_handshakes else {
            return Some(incoming);
        };
        if handshakes_in_flight < threshold
            || incoming.remote_address_validated()
            || !incoming.may_retry()
        {
            return Some(incoming);
        }
        match incoming.retry() {
            Ok(()) => {
                if let Some(counter) = &self.counter {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
            Err(err) => {
                err.into_incoming().ignore();
                None
            }
        }
    }
}

async fn accept_loop(
    endpoint: noq::Endpoint,
    site: Arc<dyn Site>,
    wt: Option<Arc<dyn WtHandler>>,
    limits: H3Limits,
    peers: Option<PeersHook>,
    admission: Admission,
    counters: Arc<Counters>,
) {
    let Admission {
        refuse_source,
        retry,
    } = admission;
    let caps = Arc::new(Mutex::new(Caps {
        total: 0,
        per_source: HashMap::new(),
        max_total: limits.max_connections,
        max_per_source: limits.max_connections_per_source,
    }));
    let handshakes = Handshakes::new(limits.max_handshakes, limits.max_handshakes_per_source);
    let runtime = tokio::runtime::Handle::current();
    while let Some(incoming) = endpoint.accept().await {
        let remote = incoming.remote_address();
        if refuse_source.as_ref().is_some_and(|filter| filter(remote)) {
            Counters::inc(&counters.sources_refused);
            incoming.refuse();
            continue;
        }
        if Caps::full(&caps, remote) {
            Counters::inc(&counters.connections_refused);
            incoming.refuse();
            continue;
        }
        // Retry before a handshake slot is taken (after the cheap
        // refusals, so a full endpoint refuses instead of Retry-then-refuse):
        // a Retried Initial costs no state, and the client's token-carrying
        // second Initial (address validated) is then admitted normally.
        let Some(incoming) = retry.apply(incoming, handshakes.in_flight()) else {
            continue;
        };
        let Some(handshake) = handshakes.try_take(remote) else {
            Counters::inc(&counters.handshakes_refused);
            incoming.refuse();
            continue;
        };
        let caps = Arc::clone(&caps);
        let site = Arc::clone(&site);
        let wt = wt.clone();
        let limits = limits.clone();
        let peers = peers.clone();
        let counters = Arc::clone(&counters);
        runtime.spawn(async move {
            let connecting = match incoming.accept() {
                Ok(connecting) => connecting,
                Err(err) => {
                    Counters::inc(&counters.handshakes_failed);
                    tracing::debug!(%remote, %err, "QUIC accept failed");
                    return;
                }
            };
            let conn = match tokio::time::timeout(limits.handshake_timeout, connecting).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(err)) => {
                    Counters::inc(&counters.handshakes_failed);
                    tracing::debug!(%remote, %err, "QUIC handshake failed");
                    return;
                }
                Err(_) => {
                    Counters::inc(&counters.handshakes_failed);
                    tracing::debug!(%remote, "QUIC handshake timed out");
                    return;
                }
            };
            drop(handshake);
            // The connection caps are charged only now: incomplete
            // handshakes never occupy an established connection's slot.
            let Some(_cap) = Caps::try_take(&caps, remote) else {
                Counters::inc(&counters.connections_refused);
                conn.close(noq::VarInt::from_u32(0x107), b"connection limit");
                return;
            };
            Counters::inc(&counters.connections_accepted);
            let _peer = peers.map(|hook| {
                hook(remote, true);
                PeerGuard { hook, remote }
            });
            if let Err(err) = serve_connection(conn, remote, site, wt, &limits, counters).await {
                tracing::debug!(%remote, %err, "h3 connection ended");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_keys() {
        let a: SocketAddr = "[2001:db8:1:2:3:4:5:6]:1".parse().unwrap();
        let b: SocketAddr = "[2001:db8:1:2:ffff::1]:2".parse().unwrap();
        assert_eq!(source_key(a), source_key(b));
        let m: SocketAddr = "[::ffff:192.0.2.1]:1".parse().unwrap();
        assert_eq!(source_key(m), "192.0.2.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn caps_global_and_per_source() {
        let caps = Arc::new(Mutex::new(Caps {
            total: 0,
            per_source: HashMap::new(),
            max_total: 3,
            max_per_source: 2,
        }));
        let a: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
        let b: SocketAddr = "[2001:db9::1]:1".parse().unwrap();
        let g1 = Caps::try_take(&caps, a).unwrap();
        let _g2 = Caps::try_take(&caps, a).unwrap();
        assert!(Caps::try_take(&caps, a).is_none());
        let _g3 = Caps::try_take(&caps, b).unwrap();
        assert!(Caps::try_take(&caps, b).is_none());
        drop(g1);
        assert!(Caps::try_take(&caps, b).is_some());
    }

    #[test]
    fn handshake_slots_are_bounded_and_released() {
        let hs = Handshakes::new(2, 1);
        let a: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
        let a2: SocketAddr = "[2001:db8::2]:1".parse().unwrap();
        let b: SocketAddr = "[2001:db9::1]:1".parse().unwrap();
        let c: SocketAddr = "[2001:dba::1]:1".parse().unwrap();
        let g = hs.try_take(a).unwrap();
        assert!(hs.try_take(a2).is_none(), "per source (same /64)");
        let _h = hs.try_take(b).unwrap();
        assert!(hs.try_take(c).is_none(), "global");
        drop(g);
        assert!(hs.try_take(c).is_some());
        assert_eq!(unpoison(hs.0.lock()).total, 1);
    }
}
