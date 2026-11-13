//! A UDP flood generator for the chaos suite (`tests/chaos_flood.rs`,
//! `tests/steering.rs`) and the `udp443_flood` example (run from a second
//! host against a real door). No extra dependencies; plain std threads.
//!
//! Shapes: garbage (reserved first byte), supported-version Initial,
//! unsupported-version Initial (a Version Negotiation trigger), short header
//! with a well-formed random 8-byte CID (a stateless-reset trigger), STUN
//! Binding Request with a bogus USERNAME, RTP. Random parts are re-drawn per
//! datagram, so every Initial and short header carries a fresh CID.
//!
//! Sources: one local address; many IPv4 loopback addresses (127.0.0.2.., Linux
//! routes all of 127/8 to `lo`; elsewhere one address); or rotating IPv6 /64s
//! under a prefix (Linux `IPV6_FREEBIND`: a socket bound to an address no
//! interface owns, sending to `[::1]`; unprivileged).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::classify::STUN_MAGIC_COOKIE;

/// A packet shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Reserved first byte (RFC 9443 drop range), random tail, 300 bytes.
    Garbage,
    /// Supported-version (v1) Initial, 1200 bytes, random keys and CIDs.
    Initial,
    /// Unsupported-version long header, 1200 bytes (draws Version Negotiation).
    VersionNegotiation,
    /// Short header with a random 8-byte CID, 60 bytes.
    Short,
    /// Well-formed STUN Binding Request with a bogus USERNAME.
    Stun,
    /// RTP, 1100 bytes.
    Rtp,
}

impl Shape {
    /// Every shape.
    pub const ALL: [Self; 6] = [
        Self::Garbage,
        Self::Initial,
        Self::VersionNegotiation,
        Self::Short,
        Self::Stun,
        Self::Rtp,
    ];
}

impl FromStr for Shape {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "garbage" => Self::Garbage,
            "initial" => Self::Initial,
            "vn" => Self::VersionNegotiation,
            "short" => Self::Short,
            "stun" => Self::Stun,
            "rtp" => Self::Rtp,
            other => return Err(format!("unknown shape {other:?}")),
        })
    }
}

/// A small xorshift generator (deterministic per seed).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seeded (0 is replaced by a fixed non-zero seed).
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Fill `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let r = self.next_u64().to_le_bytes();
            for (b, r) in chunk.iter_mut().zip(r) {
                *b = r;
            }
        }
    }
}

fn put(d: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(dst) = at
        .checked_add(bytes.len())
        .and_then(|end| d.get_mut(at..end))
    {
        dst.copy_from_slice(bytes);
    }
}

/// One datagram of `shape` with fresh random parts.
#[must_use]
pub fn datagram(shape: Shape, rng: &mut Rng) -> Vec<u8> {
    let mut d = vec![0u8; template_len(shape)];
    rng.fill(&mut d);
    shape_into(shape, &mut d);
    d
}

const fn template_len(shape: Shape) -> usize {
    match shape {
        Shape::Garbage => 300,
        Shape::Initial | Shape::VersionNegotiation => 1200,
        Shape::Short => 60,
        Shape::Stun => 40,
        Shape::Rtp => 1100,
    }
}

/// Stamp the fixed header fields of `shape` onto random bytes.
fn shape_into(shape: Shape, d: &mut [u8]) {
    match shape {
        Shape::Garbage => put(d, 0, &[0x0a]),
        Shape::Initial | Shape::VersionNegotiation => {
            let version: u32 = if shape == Shape::Initial {
                1
            } else {
                0x5a5a_5a5a
            };
            put(d, 0, &[if shape == Shape::Initial { 0xc3 } else { 0xc0 }]);
            put(d, 1, &version.to_be_bytes());
            put(d, 5, &[8]);
            put(d, 14, &[8]);
        }
        Shape::Short => put(d, 0, &[0x45]),
        Shape::Stun => {
            // Header (type 0x0001, length 20, cookie, random transaction id),
            // then USERNAME (0x0006) "abcd:efg" and 8 more bytes of padding
            // attribute so the length is a multiple of 4 and covers the rest.
            put(d, 0, &[0x00, 0x01, 0x00, 0x14]);
            put(d, 4, &STUN_MAGIC_COOKIE);
            put(d, 20, &[0x00, 0x06, 0x00, 0x08]);
            put(d, 24, b"abcd:efg");
            put(d, 32, &[0x80, 0x22, 0x00, 0x04]);
        }
        Shape::Rtp => put(d, 0, &[0x80, 96]),
    }
}

/// Where the flood comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sources {
    /// One socket bound to this local address (port 0 = ephemeral).
    One(SocketAddr),
    /// 50 IPv4 loopback addresses per thread (127.0.0.2 ..); one where the
    /// platform routes only 127.0.0.1. Sends to the target's port on
    /// 127.0.0.1 when the target is a loopback or unspecified address.
    ManyV4Loopback,
    /// IPv6 sources each in its own /64 under `prefix` (its first 32 bits
    /// are kept), rotated continuously, bound with `IPV6_FREEBIND` (Linux).
    RotatingV6 {
        /// The /32 the sources are drawn from (e.g. `2001:db8::`).
        prefix: Ipv6Addr,
    },
}

/// A flood.
#[derive(Debug, Clone)]
pub struct FloodConfig {
    /// Destination.
    pub target: SocketAddr,
    /// Datagrams per second, all threads together.
    pub pps: u64,
    /// Sender threads (at least 1).
    pub threads: usize,
    /// Source addresses.
    pub sources: Sources,
    /// Shapes, cycled.
    pub shapes: Vec<Shape>,
}

/// A running flood; stop it to join the threads.
#[derive(Debug)]
pub struct Flood {
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

/// Sockets one sender thread cycles through.
struct SourcePool {
    sockets: Vec<UdpSocket>,
    /// For rotation: the next /64 counter and the prefix words.
    rotate: Option<(Ipv6Addr, u16, u16)>,
    next: usize,
}

/// Rotating sources: sockets per thread, and how many datagrams each /64
/// sends before its socket is replaced by a fresh /64.
const ROTATING_POOL: usize = 32;
const ROTATE_EVERY: u64 = 64;

/// A freebind IPv6 socket bound to `ip` (port 0).
///
/// # Errors
///
/// Any socket error; `Unsupported` off Linux.
pub fn freebind_v6(ip: Ipv6Addr) -> io::Result<UdpSocket> {
    #[cfg(target_os = "linux")]
    {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(true)?;
        s.set_freebind_v6(true)?;
        s.bind(&SocketAddr::new(IpAddr::V6(ip), 0).into())?;
        Ok(s.into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ip;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPV6_FREEBIND is Linux only",
        ))
    }
}

/// The /64 number `n` of thread `t` under `prefix`: `prefix[0..2]:t:n::5`.
#[must_use]
pub fn rotating_source(prefix: Ipv6Addr, thread: u16, n: u16) -> Ipv6Addr {
    let p = prefix.segments();
    let (a, b) = (
        p.first().copied().unwrap_or(0x2001),
        p.get(1).copied().unwrap_or(0xdb8),
    );
    Ipv6Addr::new(a, b, thread, n, 0, 0, 0, 5)
}

/// Whether rotating IPv6 sources work here (freebind on this platform).
#[must_use]
pub fn rotating_v6_supported() -> bool {
    freebind_v6(rotating_source(
        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0),
        0xffff,
        0,
    ))
    .is_ok()
}

impl SourcePool {
    fn new(sources: &Sources, thread: u16) -> io::Result<Self> {
        let mut rotate = None;
        let sockets = match sources {
            Sources::One(addr) => vec![UdpSocket::bind(addr)?],
            Sources::ManyV4Loopback => {
                let base = u32::from(Ipv4Addr::new(127, 0, 0, 2))
                    .saturating_add(u32::from(thread).saturating_mul(50));
                let mut v: Vec<UdpSocket> = (0..50u32)
                    .filter_map(|i| {
                        let ip = Ipv4Addr::from(base.saturating_add(i));
                        if ip.octets().first() != Some(&127) {
                            return None;
                        }
                        UdpSocket::bind((ip, 0)).ok()
                    })
                    .collect();
                if v.is_empty() {
                    v.push(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?);
                }
                v
            }
            Sources::RotatingV6 { prefix } => {
                let mut v = Vec::with_capacity(ROTATING_POOL);
                let mut n = 0u16;
                for _ in 0..ROTATING_POOL {
                    v.push(freebind_v6(rotating_source(*prefix, thread, n))?);
                    n = n.wrapping_add(1);
                }
                rotate = Some((*prefix, thread, n));
                v
            }
        };
        for s in &sockets {
            s.set_nonblocking(true)?;
        }
        Ok(Self {
            sockets,
            rotate,
            next: 0,
        })
    }

    fn pick(&mut self) -> Option<&UdpSocket> {
        let len = self.sockets.len().max(1);
        self.next = (self.next + 1) % len;
        self.sockets.get(self.next)
    }

    /// Replace one socket with a fresh /64 (rotating sources only).
    fn rotate(&mut self) {
        let Some((prefix, thread, n)) = self.rotate.as_mut() else {
            return;
        };
        if let Ok(s) = freebind_v6(rotating_source(*prefix, *thread, *n)) {
            let _ = s.set_nonblocking(true);
            let slot = usize::from(*n) % self.sockets.len().max(1);
            if let Some(old) = self.sockets.get_mut(slot) {
                *old = s;
            }
        }
        *n = n.wrapping_add(1);
    }
}

/// Where a flood from `sources` must be sent to reach `target`.
fn effective_target(target: SocketAddr, sources: &Sources) -> SocketAddr {
    let loopbackish = target.ip().is_loopback() || target.ip().is_unspecified();
    match sources {
        Sources::ManyV4Loopback if loopbackish => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), target.port())
        }
        Sources::One(local) if loopbackish && local.is_ipv4() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), target.port())
        }
        Sources::RotatingV6 { .. } | Sources::One(_) if loopbackish => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), target.port())
        }
        _ => target,
    }
}

impl Flood {
    /// Start the sender threads. Their sockets are created before this
    /// returns, so a source setup error is reported here.
    ///
    /// # Errors
    ///
    /// A socket error for the requested sources (e.g. freebind unsupported).
    pub fn start(config: FloodConfig) -> io::Result<Self> {
        let threads = config.threads.max(1);
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let target = effective_target(config.target, &config.sources);
        let FloodConfig {
            target: _,
            pps,
            threads: _,
            sources,
            shapes,
        } = config;
        let shapes = if shapes.is_empty() {
            Shape::ALL.to_vec()
        } else {
            shapes
        };
        let per_thread = pps
            .checked_div(u64::try_from(threads).unwrap_or(1))
            .unwrap_or(0)
            .max(1);
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            let thread = u16::try_from(t).unwrap_or(u16::MAX);
            let mut pool = SourcePool::new(&sources, thread)?;
            let stop = Arc::clone(&stop);
            let sent = Arc::clone(&sent);
            let shapes = shapes.clone();
            handles.push(std::thread::spawn(move || {
                send_loop(&mut pool, target, &shapes, per_thread, thread, &stop, &sent);
            }));
        }
        Ok(Self {
            stop,
            sent,
            threads: handles,
        })
    }

    /// Datagrams handed to the kernel so far.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Stop and join; returns the datagrams sent.
    #[must_use]
    pub fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
        self.sent.load(Ordering::Relaxed)
    }
}

fn send_loop(
    pool: &mut SourcePool,
    target: SocketAddr,
    shapes: &[Shape],
    pps: u64,
    thread: u16,
    stop: &AtomicBool,
    sent: &AtomicU64,
) {
    const BATCH: u64 = 64;
    let mut rng = Rng::new(0x5eed_0000_u64 ^ u64::from(thread).wrapping_mul(0x9e37_79b9));
    let mut bufs: Vec<(Shape, Vec<u8>)> =
        shapes.iter().map(|&s| (s, datagram(s, &mut rng))).collect();
    let interval = Duration::from_nanos(
        1_000_000_000u64
            .saturating_mul(BATCH)
            .checked_div(pps)
            .unwrap_or(1),
    );
    let mut next = Instant::now();
    let mut n = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let mut ok = 0u64;
        for _ in 0..BATCH {
            let idx =
                usize::try_from(n % u64::try_from(bufs.len().max(1)).unwrap_or(1)).unwrap_or(0);
            if let Some((shape, buf)) = bufs.get_mut(idx) {
                // Fresh random bytes where they matter (CIDs, transaction id).
                match shape {
                    Shape::Initial | Shape::VersionNegotiation => {
                        if let Some(cid) = buf.get_mut(6..14) {
                            rng.fill(cid);
                        }
                    }
                    Shape::Short => {
                        if let Some(cid) = buf.get_mut(1..9) {
                            rng.fill(cid);
                        }
                    }
                    Shape::Stun => {
                        if let Some(tid) = buf.get_mut(8..20) {
                            rng.fill(tid);
                        }
                    }
                    Shape::Garbage | Shape::Rtp => {}
                }
                if let Some(s) = pool.pick() {
                    if s.send_to(buf, target).is_ok() {
                        ok += 1;
                    }
                }
            }
            n = n.wrapping_add(1);
            if n % ROTATE_EVERY == 0 {
                pool.rotate();
            }
        }
        sent.fetch_add(ok, Ordering::Relaxed);
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now.saturating_duration_since(next) > Duration::from_millis(100) {
            next = now;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::classify::{classify, QuicKind, Verdict};

    #[test]
    fn shapes_classify_as_intended() {
        let mut rng = Rng::new(1);
        for _ in 0..50 {
            assert!(matches!(
                classify(&datagram(Shape::Garbage, &mut rng), false),
                Verdict::Drop(_)
            ));
            assert_eq!(
                classify(&datagram(Shape::Initial, &mut rng), false),
                Verdict::Quic(QuicKind::Initial)
            );
            assert_eq!(
                classify(&datagram(Shape::VersionNegotiation, &mut rng), false),
                Verdict::Quic(QuicKind::VersionNegotiationTrigger)
            );
            assert_eq!(
                classify(&datagram(Shape::Short, &mut rng), false),
                Verdict::Quic(QuicKind::Short)
            );
            let stun = datagram(Shape::Stun, &mut rng);
            assert_eq!(classify(&stun, false), Verdict::Rtc);
            assert!(crate::classify::is_stun(&stun));
            assert_eq!(
                classify(&datagram(Shape::Rtp, &mut rng), false),
                Verdict::Rtc
            );
        }
        assert_eq!("vn".parse::<Shape>(), Ok(Shape::VersionNegotiation));
        assert!("x".parse::<Shape>().is_err());
    }

    #[test]
    fn rotating_sources_are_distinct_slash_64s() {
        let p: Ipv6Addr = "2001:db8::".parse().unwrap();
        let a = rotating_source(p, 1, 2);
        let b = rotating_source(p, 1, 3);
        let c = rotating_source(p, 2, 2);
        let key = |ip: Ipv6Addr| ip.segments()[..4].to_vec();
        assert_ne!(key(a), key(b));
        assert_ne!(key(a), key(c));
        assert_eq!(a.segments()[0], 0x2001);
    }
}
