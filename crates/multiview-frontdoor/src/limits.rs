//! Bounded state for the dispatcher (design note sections 3 and 13).
//!
//! * [`TokenBucket`]: integer token bucket (no floats on the hot path).
//! * [`NewFlowLimiter`]: per-source buckets in a fixed-capacity LRU, a global
//!   new-flow budget on top, and a separate global budget for datagrams that can
//!   draw a QUIC Version Negotiation reply. Owned by the one dispatcher task, so
//!   it needs no lock. Checked **before** any allocation for the datagram.
//!   Well-formed STUN from an unvalidated source pays its own per-source and
//!   global STUN budgets first (issue #289).
//! * [`PeerBuckets`]: a generous per-established-peer bucket, so one spoofed
//!   established 4-tuple cannot take the whole established lane queue
//!   (issue #325). Bounded by the established-set caps.
//! * [`EstablishedSet`]: the validated 4-tuples (remote addresses), written only
//!   by the consumers after validation, capped, never cleared by traffic. The
//!   dispatcher reads it with `try_read` and treats a contended lock as "not
//!   established", so it never blocks.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Instant;

use hashlink::LruCache;

use crate::classify::{canonical, SourceKey};

/// Token units per whole token (sub-token precision without floats).
const SCALE: u128 = 1_000;

/// An integer token bucket refilled from elapsed monotonic time.
#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    /// Current tokens, in 1/[`SCALE`] units.
    tokens: u128,
    /// Capacity, in 1/[`SCALE`] units.
    burst: u128,
    /// Refill rate in whole tokens per second.
    rate: u128,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket.
    #[must_use]
    pub fn new(rate_per_sec: u32, burst: u32, now: Instant) -> Self {
        let burst = u128::from(burst.max(1)) * SCALE;
        Self {
            tokens: burst,
            burst,
            rate: u128::from(rate_per_sec),
            last: now,
        }
    }

    /// Take one token if available.
    pub fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_nanos();
        if elapsed > 0 {
            // tokens += elapsed_s * rate, in SCALE units.
            let add = elapsed.saturating_mul(self.rate).saturating_mul(SCALE) / 1_000_000_000;
            if add > 0 {
                self.tokens = self.tokens.saturating_add(add).min(self.burst);
                self.last = now;
            }
        }
        if self.tokens >= SCALE {
            self.tokens -= SCALE;
            true
        } else {
            false
        }
    }
}

/// New-flow limits (design note section 10, `[network.udp]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewFlowLimits {
    /// Sustained datagrams per second per source key.
    pub per_source_pps: u32,
    /// Burst per source key.
    pub per_source_burst: u32,
    /// Sustained new-flow datagrams per second for all sources together.
    pub global_pps: u32,
    /// Most source keys tracked at once (LRU beyond).
    pub max_sources: usize,
    /// Sustained Version-Negotiation-trigger datagrams per second, all sources.
    pub vn_pps: u32,
    /// Sustained supported-version Initials per second from unvalidated
    /// addresses, all sources: bounds how fast noq can be made to create
    /// handshake state (each pending handshake lives until its deadline).
    pub initial_pps: u32,
    /// Sustained well-formed STUN messages per second per source key from
    /// unvalidated sources. An ICE agent paces checks at Ta >= 50 ms per
    /// candidate pair, so 50/s fits a few pairs.
    pub stun_per_source_pps: u32,
    /// STUN burst per source key.
    pub stun_per_source_burst: u32,
    /// Sustained STUN messages per second from unvalidated sources, all
    /// sources together (burst: a tenth of a second).
    pub stun_global_pps: u32,
    /// Sustained datagrams per second per established peer (generous: a
    /// 4K WebRTC stream is ~1 500/s). Bounds what one spoofed established
    /// 4-tuple can push into the established queue (issue #325).
    pub established_peer_pps: u32,
    /// Burst per established peer.
    pub established_peer_burst: u32,
}

impl Default for NewFlowLimits {
    fn default() -> Self {
        Self {
            per_source_pps: 500,
            per_source_burst: 200,
            global_pps: 50_000,
            max_sources: 16_384,
            vn_pps: 200,
            initial_pps: 200,
            stun_per_source_pps: 50,
            stun_per_source_burst: 50,
            stun_global_pps: 5_000,
            established_peer_pps: 20_000,
            established_peer_burst: 4_000,
        }
    }
}

/// Why the limiter refused a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The source's own bucket is empty.
    PerSource,
    /// The global new-flow budget is spent.
    Global,
    /// The global Version Negotiation budget is spent.
    VersionNegotiation,
    /// The global handshake (Initial) budget is spent.
    Initial,
    /// The source's STUN bucket is empty.
    StunPerSource,
    /// The global STUN budget is spent.
    StunGlobal,
}

/// Which extra global budget a new-flow datagram pays, besides the
/// per-source and global new-flow budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// None.
    None,
    /// The Version Negotiation budget.
    VersionNegotiation,
    /// The handshake (Initial) budget.
    Initial,
    /// The per-source and global STUN budgets (a well-formed STUN message
    /// from a non-established source).
    Stun,
}

/// Per-source buckets (bounded LRU) plus global budgets. Single owner.
#[derive(Debug)]
pub struct NewFlowLimiter {
    limits: NewFlowLimits,
    sources: LruCache<SourceKey, TokenBucket>,
    global: TokenBucket,
    vn: TokenBucket,
    initial: TokenBucket,
    stun_sources: LruCache<SourceKey, TokenBucket>,
    stun_global: TokenBucket,
}

impl NewFlowLimiter {
    /// An empty limiter with full global buckets.
    #[must_use]
    pub fn new(limits: NewFlowLimits, now: Instant) -> Self {
        let max_sources = limits.max_sources.max(1);
        Self {
            limits,
            sources: LruCache::new(max_sources),
            // A tenth of a second of burst on the global budgets.
            global: TokenBucket::new(limits.global_pps, (limits.global_pps / 10).max(1), now),
            vn: TokenBucket::new(limits.vn_pps, (limits.vn_pps / 10).max(1), now),
            initial: TokenBucket::new(limits.initial_pps, (limits.initial_pps / 10).max(1), now),
            stun_sources: LruCache::new(max_sources),
            stun_global: TokenBucket::new(
                limits.stun_global_pps,
                (limits.stun_global_pps / 10).max(1),
                now,
            ),
        }
    }

    /// Admit one new-flow datagram from `src`, or say why not. A Version
    /// Negotiation trigger (or an Initial) is checked against its global budget
    /// **first**, so a rotating-source flood of unknown-version long headers
    /// draws at most `vn_pps` replies, and of Initials creates at most
    /// `initial_pps` handshakes per second, whatever the per-source and global
    /// budgets allow. Then
    /// the per-source bucket, so a single noisy source cannot spend the global
    /// budget beyond its own rate, then the global budget. A STUN message
    /// pays its own per-source STUN bucket and the global STUN budget first,
    /// so a refused STUN message spends nothing else.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] naming the budget that is spent.
    pub fn admit(&mut self, src: SocketAddr, budget: Budget, now: Instant) -> Result<(), Refusal> {
        match budget {
            Budget::VersionNegotiation if !self.vn.try_take(now) => {
                return Err(Refusal::VersionNegotiation);
            }
            Budget::Initial if !self.initial.try_take(now) => return Err(Refusal::Initial),
            Budget::Stun => self.admit_stun(src, now)?,
            _ => {}
        }
        let key = SourceKey::of(src);
        let per_source = if let Some(bucket) = self.sources.get_mut(&key) {
            bucket.try_take(now)
        } else {
            // Insert evicts the least recently used key when full, so the
            // table never grows past `max_sources`; an attacker minting keys
            // evicts their own entries first (they are the least recent).
            let mut bucket = TokenBucket::new(
                self.limits.per_source_pps,
                self.limits.per_source_burst,
                now,
            );
            let ok = bucket.try_take(now);
            self.sources.insert(key, bucket);
            ok
        };
        if !per_source {
            return Err(Refusal::PerSource);
        }
        if !self.global.try_take(now) {
            return Err(Refusal::Global);
        }
        Ok(())
    }

    fn admit_stun(&mut self, src: SocketAddr, now: Instant) -> Result<(), Refusal> {
        let key = SourceKey::of(src);
        let per_source = if let Some(bucket) = self.stun_sources.get_mut(&key) {
            bucket.try_take(now)
        } else {
            let mut bucket = TokenBucket::new(
                self.limits.stun_per_source_pps,
                self.limits.stun_per_source_burst,
                now,
            );
            let ok = bucket.try_take(now);
            self.stun_sources.insert(key, bucket);
            ok
        };
        if !per_source {
            return Err(Refusal::StunPerSource);
        }
        if !self.stun_global.try_take(now) {
            return Err(Refusal::StunGlobal);
        }
        Ok(())
    }

    /// Charge a supported-version Initial from an **established** source
    /// address: the handshake budget and the global new-flow budget, never
    /// the per-source bucket (issue #325: the address alone is spoofable, so
    /// an Initial never bypasses the budgets that bound handshake state).
    ///
    /// # Errors
    ///
    /// [`Refusal::Initial`] or [`Refusal::Global`].
    pub fn admit_initial_established(&mut self, now: Instant) -> Result<(), Refusal> {
        if !self.initial.try_take(now) {
            return Err(Refusal::Initial);
        }
        if !self.global.try_take(now) {
            return Err(Refusal::Global);
        }
        Ok(())
    }

    /// STUN source keys currently tracked (never more than `max_sources`).
    #[must_use]
    pub fn tracked_stun_sources(&self) -> usize {
        self.stun_sources.len()
    }

    /// Source keys currently tracked (never more than `max_sources`).
    #[must_use]
    pub fn tracked_sources(&self) -> usize {
        self.sources.len()
    }

    /// The configured cap on tracked source keys.
    #[must_use]
    pub fn max_sources(&self) -> usize {
        self.sources.capacity()
    }
}

/// The validated remote addresses, bounded by `cap` (the session and
/// connection caps). Written only by the consumers after validation (ICE
/// connected with MESSAGE-INTEGRITY; a confirmed QUIC handshake), read by the
/// dispatcher without blocking. Lives entirely on the front-door runtime: no
/// program-runtime task ever touches this lock.
///
/// Every insert that adds a new address and every remove that deletes one
/// bumps a generation counter and wakes the steering updater
/// ([`changed`](Self::changed)); the dispatcher never waits on it.
#[derive(Debug)]
pub struct EstablishedSet {
    cap: usize,
    addrs: RwLock<HashMap<SocketAddr, u32>>,
    generation: AtomicU64,
    changed: tokio::sync::Notify,
}

impl EstablishedSet {
    /// An empty set holding at most `cap` addresses.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            addrs: RwLock::new(HashMap::with_capacity(cap.min(4096))),
            generation: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }

    /// Whether `addr` is established. Never blocks: a contended or poisoned
    /// lock reads as "not established", which only means the datagram takes
    /// the new-flow path for that moment.
    #[must_use]
    pub fn contains(&self, addr: SocketAddr) -> bool {
        let addr = canonical(addr);
        // Writers hold the lock for one map operation; retry a few times
        // (spinning, never parking) before treating the peer as new.
        for _ in 0..4 {
            match self.addrs.try_read() {
                Ok(set) => return set.contains_key(&addr),
                Err(std::sync::TryLockError::WouldBlock) => std::hint::spin_loop(),
                Err(std::sync::TryLockError::Poisoned(_)) => return false,
            }
        }
        false
    }

    /// Mark `addr` validated (one more reference). Returns `false` (and
    /// changes nothing) when a new address would exceed the cap: a consumer that
    /// validated more peers than the session caps allow is a bug, and the extra
    /// peer stays on the rate-limited path. Pair every `true` with one
    /// [`remove`](Self::remove).
    pub fn insert(&self, addr: SocketAddr) -> bool {
        let Ok(mut set) = self.addrs.write() else {
            return false;
        };
        let addr = canonical(addr);
        if let Some(refs) = set.get_mut(&addr) {
            *refs = refs.saturating_add(1);
            return true;
        }
        if set.len() >= self.cap {
            return false;
        }
        set.insert(addr, 1);
        drop(set);
        self.bump();
        true
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        // A stored permit: an updater that is busy compiling sees it next.
        self.changed.notify_one();
    }

    /// Bumped on every membership change (an address added or removed).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Resolves after the next membership change (or at once if one happened
    /// since the last wait). For the steering updater, never the dispatcher.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    /// A copy of the addresses (bounded by the cap). Takes the read lock
    /// (writers hold it for one map operation); for the steering updater,
    /// never the dispatcher.
    #[must_use]
    pub fn snapshot(&self) -> Vec<SocketAddr> {
        self.addrs
            .read()
            .map(|s| s.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Drop one reference to `addr` (its session or connection ended); the
    /// address leaves the set with its last reference.
    pub fn remove(&self, addr: SocketAddr) {
        if let Ok(mut set) = self.addrs.write() {
            let addr = canonical(addr);
            if let Some(refs) = set.get_mut(&addr) {
                *refs = refs.saturating_sub(1);
                if *refs == 0 {
                    set.remove(&addr);
                    drop(set);
                    self.bump();
                }
            }
        }
    }

    /// How many addresses are established (diagnostics; never blocks: a
    /// contended lock reads as 0).
    #[must_use]
    pub fn len(&self) -> usize {
        self.addrs.try_read().map_or(0, |s| s.len())
    }

    /// Whether no address is established.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The cap.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }
}

/// Per-established-peer token buckets, owned by the dispatcher (no lock).
/// Bounded: at most `cap` entries (the sum of the established-set caps), LRU
/// beyond; entries whose address left the established sets are pruned when
/// the sets' generation changes.
#[derive(Debug)]
pub struct PeerBuckets {
    pps: u32,
    burst: u32,
    buckets: LruCache<SocketAddr, TokenBucket>,
}

impl PeerBuckets {
    /// An empty table of at most `cap` peers.
    #[must_use]
    pub fn new(limits: &NewFlowLimits, cap: usize) -> Self {
        Self {
            pps: limits.established_peer_pps,
            burst: limits.established_peer_burst,
            buckets: LruCache::new(cap.max(1)),
        }
    }

    /// Take one token from `peer`'s bucket.
    pub fn admit(&mut self, peer: SocketAddr, now: Instant) -> bool {
        let peer = canonical(peer);
        if let Some(b) = self.buckets.get_mut(&peer) {
            return b.try_take(now);
        }
        let mut b = TokenBucket::new(self.pps, self.burst, now);
        let ok = b.try_take(now);
        self.buckets.insert(peer, b);
        ok
    }

    /// Drop the buckets of peers `keep` no longer accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(SocketAddr) -> bool) {
        self.buckets.retain(|addr, _| keep(*addr));
    }

    /// Peers tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no peer is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    #[test]
    fn bucket_bursts_then_refills_at_rate() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(100, 10, t0);
        let took = (0..50).filter(|_| b.try_take(t0)).count();
        assert_eq!(took, 10, "burst");
        // 100 ms at 100/s refills 10.
        let t1 = t0 + Duration::from_millis(100);
        let took = (0..50).filter(|_| b.try_take(t1)).count();
        assert_eq!(took, 10, "refill");
        // Refill never exceeds burst.
        let t2 = t1 + Duration::from_secs(60);
        let took = (0..50).filter(|_| b.try_take(t2)).count();
        assert_eq!(took, 10, "capped at burst");
    }

    #[test]
    fn one_source_is_held_to_its_own_rate() {
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(NewFlowLimits::default(), t0);
        let src: SocketAddr = "[2001:db8::1]:4000".parse().unwrap();
        let ok = (0..10_000)
            .filter(|_| l.admit(src, Budget::None, t0).is_ok())
            .count();
        assert_eq!(ok, 200, "only the per-source burst passes");
        // Another port in the same /64 is the same source.
        let same64: SocketAddr = "[2001:db8::ffff]:9".parse().unwrap();
        assert_eq!(l.admit(same64, Budget::None, t0), Err(Refusal::PerSource));
        assert_eq!(l.tracked_sources(), 1);
    }

    #[test]
    fn n_distinct_spoofed_sources_cannot_grow_the_table_past_its_cap() {
        let limits = NewFlowLimits {
            max_sources: 1024,
            ..NewFlowLimits::default()
        };
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(limits, t0);
        let mut admitted = 0usize;
        // 200 000 distinct IPv4 sources and 200 000 distinct IPv6 /64s.
        for i in 0..200_000u32 {
            let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)), 1234);
            let o = i.to_be_bytes();
            let v6 = SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(
                    0x2001,
                    0xdb8,
                    u16::from_be_bytes([o[0], o[1]]),
                    u16::from_be_bytes([o[2], o[3]]),
                    0,
                    0,
                    0,
                    1,
                )),
                1234,
            );
            for src in [v4, v6] {
                admitted += usize::from(l.admit(src, Budget::None, t0).is_ok());
                assert!(l.tracked_sources() <= 1024);
            }
        }
        assert_eq!(l.tracked_sources(), 1024);
        assert_eq!(l.max_sources(), 1024);
        // The global budget bounds total userspace work for a spoofed flood.
        assert_eq!(admitted, 5_000, "global burst = global_pps / 10");
    }

    #[test]
    fn version_negotiation_triggers_have_their_own_global_budget() {
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(NewFlowLimits::default(), t0);
        let mut vn_ok = 0;
        for i in 0..1000u32 {
            let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(0xc000_0200 + i)), 1);
            if l.admit(src, Budget::VersionNegotiation, t0).is_ok() {
                vn_ok += 1;
            }
        }
        assert_eq!(vn_ok, 20, "vn burst = vn_pps / 10");
    }

    #[test]
    fn vn_budget_binds_before_per_source_and_global_budgets() {
        // Review finding (PR 315): with the VN check last, a rotating-source
        // flood spent the 50 000/s global budget, not the 200/s VN budget.
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(NewFlowLimits::default(), t0);
        let mut vn = 0usize;
        let mut refused_vn = 0usize;
        for i in 0..5_000u32 {
            let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)), 443);
            match l.admit(src, Budget::VersionNegotiation, t0) {
                Ok(()) => vn += 1,
                Err(Refusal::VersionNegotiation) => refused_vn += 1,
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(vn, 20);
        assert_eq!(refused_vn, 4_980);
        // Refused VN triggers spent nothing else: no source keys were created and
        // ordinary new flows still get the whole global burst.
        assert_eq!(l.tracked_sources(), 20);
        // One second later the VN budget has refilled by exactly vn_pps (capped
        // at its burst).
        let t1 = t0 + Duration::from_secs(1);
        let again = (0..1_000u32)
            .filter(|i| {
                let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + i)), 443);
                l.admit(src, Budget::VersionNegotiation, t1).is_ok()
            })
            .count();
        assert_eq!(again, 20);
    }

    #[test]
    fn initials_from_rotating_sources_are_held_to_the_handshake_budget() {
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(NewFlowLimits::default(), t0);
        let ok = (0..10_000u32)
            .filter(|i| {
                let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(0x0c00_0000 + i)), 443);
                l.admit(src, Budget::Initial, t0).is_ok()
            })
            .count();
        assert_eq!(ok, 20, "initial burst = initial_pps / 10");
        // Other new-flow traffic is unaffected by a spent handshake budget.
        let src: SocketAddr = "[2001:db8::7]:5".parse().unwrap();
        assert!(l.admit(src, Budget::None, t0).is_ok());
    }

    #[test]
    fn one_source_is_held_to_its_stun_burst() {
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(NewFlowLimits::default(), t0);
        let src: SocketAddr = "[2001:db8:5::1]:3478".parse().unwrap();
        let mut ok = 0;
        let mut per_source = 0;
        for _ in 0..1_000 {
            match l.admit(src, Budget::Stun, t0) {
                Ok(()) => ok += 1,
                Err(Refusal::StunPerSource) => per_source += 1,
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(ok, 50, "stun_per_source_burst");
        assert_eq!(per_source, 950);
        // A refused STUN message spent nothing of the generic per-source
        // bucket: the same source still gets its whole 200 burst for other
        // new-flow traffic.
        let other = (0..1_000)
            .filter(|_| l.admit(src, Budget::None, t0).is_ok())
            .count();
        assert_eq!(
            other,
            200 - 50,
            "the 50 admitted STUN also paid the generic bucket"
        );
        // 100 ms later: 5 more STUN tokens (50/s).
        let t1 = t0 + Duration::from_millis(100);
        let again = (0..100)
            .filter(|_| l.admit(src, Budget::Stun, t1).is_ok())
            .count();
        assert!(again <= 5, "{again}");
    }

    #[test]
    fn rotating_slash_64s_are_held_to_the_global_stun_burst() {
        let limits = NewFlowLimits {
            max_sources: 1024,
            ..NewFlowLimits::default()
        };
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(limits, t0);
        let mut ok = 0usize;
        let mut global = 0usize;
        for i in 0..50_000u32 {
            let o = i.to_be_bytes();
            let src = SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(
                    0x2001,
                    0xdb8,
                    u16::from_be_bytes([o[0], o[1]]),
                    u16::from_be_bytes([o[2], o[3]]),
                    0,
                    0,
                    0,
                    1,
                )),
                5000,
            );
            match l.admit(src, Budget::Stun, t0) {
                Ok(()) => ok += 1,
                Err(Refusal::StunGlobal) => global += 1,
                Err(other) => panic!("unexpected {other:?}"),
            }
            assert!(l.tracked_stun_sources() <= 1024, "table never above cap");
        }
        assert_eq!(ok, 500, "global STUN burst = stun_global_pps / 10");
        assert_eq!(global, 49_500);
        assert_eq!(l.tracked_stun_sources(), 1024);
        // Other traffic is unaffected by a spent STUN budget.
        let src: SocketAddr = "[2001:db8:ffff::1]:5".parse().unwrap();
        assert!(l.admit(src, Budget::None, t0).is_ok());
        assert!(l.admit(src, Budget::Initial, t0).is_ok());
    }

    #[test]
    fn an_initial_from_an_established_address_pays_initial_and_global() {
        let t0 = Instant::now();
        let mut l = NewFlowLimiter::new(NewFlowLimits::default(), t0);
        let ok = (0..2_000)
            .filter(|_| l.admit_initial_established(t0).is_ok())
            .count();
        assert_eq!(ok, 20, "initial burst");
        assert_eq!(l.tracked_sources(), 0, "no per-source key");
    }

    #[test]
    fn peer_buckets_bound_one_peer_and_the_table() {
        let t0 = Instant::now();
        let limits = NewFlowLimits::default();
        let mut b = PeerBuckets::new(&limits, 4);
        let peer: SocketAddr = "192.0.2.9:4000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.0.2.9]:4000".parse().unwrap();
        let ok = (0..10_000).filter(|_| b.admit(mapped, t0)).count();
        assert_eq!(ok, 4_000, "established_peer_burst");
        assert!(!b.admit(peer, t0), "mapped and plain v4 are one peer");
        for i in 0..10u16 {
            b.admit(SocketAddr::new(peer.ip(), i), t0);
            assert!(b.len() <= 4);
        }
        b.retain(|a| a.port() == 9);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn established_set_changes_bump_the_generation() {
        let set = EstablishedSet::new(4);
        let a: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
        let g0 = set.generation();
        assert!(set.insert(a));
        assert_eq!(set.generation(), g0 + 1);
        assert!(set.insert(a), "second reference");
        assert_eq!(set.generation(), g0 + 1, "no membership change");
        set.remove(a);
        assert_eq!(set.generation(), g0 + 1, "one reference left");
        set.remove(a);
        assert_eq!(set.generation(), g0 + 2);
        assert_eq!(set.snapshot(), Vec::<SocketAddr>::new());
        // The stored permit: a waiter after the change resolves at once.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), set.changed())
                .await
                .unwrap();
        });
    }

    #[test]
    fn established_set_is_capped_and_mapped_v4_is_one_key() {
        let set = EstablishedSet::new(2);
        let a: SocketAddr = "192.0.2.1:1000".parse().unwrap();
        let a_mapped: SocketAddr = "[::ffff:192.0.2.1]:1000".parse().unwrap();
        let b: SocketAddr = "[2001:db8::2]:1000".parse().unwrap();
        let c: SocketAddr = "[2001:db8::3]:1000".parse().unwrap();
        assert!(set.insert(a));
        assert!(set.contains(a_mapped));
        assert!(set.insert(a_mapped), "same key, no growth");
        assert!(set.insert(b));
        assert!(!set.insert(c), "cap holds");
        assert_eq!(set.len(), 2);
        set.remove(a_mapped);
        assert!(set.contains(a), "one reference left");
        set.remove(a);
        assert!(!set.contains(a));
        assert!(set.insert(c));
    }
}
