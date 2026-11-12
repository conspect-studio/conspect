//! Bounded state for the dispatcher (design note sections 3 and 13).
//!
//! * [`TokenBucket`]: integer token bucket (no floats on the hot path).
//! * [`NewFlowLimiter`]: per-source buckets in a fixed-capacity LRU, a global
//!   new-flow budget on top, and a separate global budget for datagrams that can
//!   draw a QUIC Version Negotiation reply. Owned by the one dispatcher task, so
//!   it needs no lock. Checked **before** any allocation for the datagram.
//! * [`EstablishedSet`]: the validated 4-tuples (remote addresses), written only
//!   by the consumers after validation, capped, never cleared by traffic. The
//!   dispatcher reads it with `try_read` and treats a contended lock as "not
//!   established", so it never blocks.

use std::collections::HashMap;
use std::net::SocketAddr;
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
}

/// Per-source buckets (bounded LRU) plus global budgets. Single owner.
#[derive(Debug)]
pub struct NewFlowLimiter {
    limits: NewFlowLimits,
    sources: LruCache<SourceKey, TokenBucket>,
    global: TokenBucket,
    vn: TokenBucket,
    initial: TokenBucket,
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
        }
    }

    /// Admit one new-flow datagram from `src`, or say why not. A Version
    /// Negotiation trigger (or an Initial) is checked against its global budget
    /// **first**, so a rotating-source flood of unknown-version long headers
    /// draws at most `vn_pps` replies, and of Initials creates at most
    /// `initial_pps` handshakes per second, whatever the per-source and global
    /// budgets allow. Then
    /// the per-source bucket, so a single noisy source cannot spend the global
    /// budget beyond its own rate, then the global budget.
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
#[derive(Debug)]
pub struct EstablishedSet {
    cap: usize,
    addrs: RwLock<HashMap<SocketAddr, u32>>,
}

impl EstablishedSet {
    /// An empty set holding at most `cap` addresses.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            addrs: RwLock::new(HashMap::with_capacity(cap.min(4096))),
        }
    }

    /// Whether `addr` is established. Never blocks: a contended or poisoned
    /// lock reads as "not established", which only means the datagram takes
    /// the new-flow path for that moment.
    #[must_use]
    pub fn contains(&self, addr: SocketAddr) -> bool {
        match self.addrs.try_read() {
            Ok(set) => set.contains_key(&canonical(addr)),
            Err(_) => false,
        }
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
        true
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
                }
            }
        }
    }

    /// How many addresses are established.
    #[must_use]
    pub fn len(&self) -> usize {
        self.addrs.read().map_or(0, |s| s.len())
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
