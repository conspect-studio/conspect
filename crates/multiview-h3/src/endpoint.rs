//! The noq endpoint on a caller-provided socket, its accept loop and caps.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
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
                let accept = tokio::spawn(accept_loop(
                    endpoint.clone(),
                    site,
                    wt,
                    limits,
                    peers,
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

async fn accept_loop(
    endpoint: noq::Endpoint,
    site: Arc<dyn Site>,
    wt: Option<Arc<dyn WtHandler>>,
    limits: H3Limits,
    peers: Option<PeersHook>,
    counters: Arc<Counters>,
) {
    let caps = Arc::new(Mutex::new(Caps {
        total: 0,
        per_source: HashMap::new(),
        max_total: limits.max_connections,
        max_per_source: limits.max_connections_per_source,
    }));
    while let Some(incoming) = endpoint.accept().await {
        let remote = incoming.remote_address();
        let Some(cap) = Caps::try_take(&caps, remote) else {
            Counters::inc(&counters.connections_refused);
            incoming.refuse();
            continue;
        };
        let site = Arc::clone(&site);
        let wt = wt.clone();
        let limits = limits.clone();
        let peers = peers.clone();
        let counters = Arc::clone(&counters);
        tokio::spawn(async move {
            let _cap = cap;
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
}
