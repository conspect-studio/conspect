//! Program tick **lateness** (ADR-0108 §10, the Program protection bound):
//! emit time minus the tick's deadline, measured where the runtime publishes a
//! fresh or repeated tick.
//!
//! Wait-free on the clock (invariant #1): recording is a handful of
//! `fetch_add`/`fetch_max` on fixed atomics — no allocation, no lock, no
//! syscall beyond the time source read the loop already makes. Readers on
//! other threads take a [`LatenessSnapshot`] (cumulative counters); windowed
//! percentiles are the difference of two snapshots.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Upper bucket edges in nanoseconds: 0.5, 1, 2, 4, 8, 16.7, 33.4, 66.8 ms;
/// a final overflow bucket holds everything later.
pub const LATENESS_EDGES_NS: [u64; 8] = [
    500_000, 1_000_000, 2_000_000, 4_000_000, 8_000_000, 16_700_000, 33_400_000, 66_800_000,
];
/// Buckets: one per edge plus the overflow.
pub const LATENESS_BUCKETS: usize = LATENESS_EDGES_NS.len() + 1;

/// The shared wait-free histogram.
#[derive(Debug, Default)]
pub struct TickLateness {
    buckets: [AtomicU64; LATENESS_BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

/// A point-in-time copy of [`TickLateness`] (cumulative).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatenessSnapshot {
    /// Per-bucket counts (edges [`LATENESS_EDGES_NS`], then overflow).
    pub buckets: [u64; LATENESS_BUCKETS],
    /// Ticks recorded.
    pub count: u64,
    /// Sum of lateness, nanoseconds.
    pub sum_ns: u64,
    /// Largest lateness seen, nanoseconds (cumulative, never windowed).
    pub max_ns: u64,
}

impl TickLateness {
    /// A fresh shared histogram.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record one tick emitted at `now_ns` against `deadline_ns`. Early emits
    /// count as zero lateness. Wait-free.
    pub fn record(&self, now_ns: i64, deadline_ns: i64) {
        let late = u64::try_from(now_ns.saturating_sub(deadline_ns)).unwrap_or(0);
        let index = LATENESS_EDGES_NS
            .iter()
            .position(|edge| late <= *edge)
            .unwrap_or(LATENESS_EDGES_NS.len());
        if let Some(bucket) = self.buckets.get(index) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_ns.fetch_add(late, Ordering::Relaxed);
        self.max_ns.fetch_max(late, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Release);
    }

    /// The cumulative counters now.
    #[must_use]
    pub fn snapshot(&self) -> LatenessSnapshot {
        let count = self.count.load(Ordering::Acquire);
        let mut buckets = [0; LATENESS_BUCKETS];
        for (out, bucket) in buckets.iter_mut().zip(&self.buckets) {
            *out = bucket.load(Ordering::Relaxed);
        }
        LatenessSnapshot {
            buckets,
            count,
            sum_ns: self.sum_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
        }
    }
}

impl LatenessSnapshot {
    /// The counts recorded since `earlier` (a window).
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        let mut buckets = [0; LATENESS_BUCKETS];
        for ((out, now), then) in buckets.iter_mut().zip(&self.buckets).zip(&earlier.buckets) {
            *out = now.saturating_sub(*then);
        }
        Self {
            buckets,
            count: self.count.saturating_sub(earlier.count),
            sum_ns: self.sum_ns.saturating_sub(earlier.sum_ns),
            max_ns: self.max_ns,
        }
    }

    /// The upper edge (ns) of the bucket holding the quantile `permille`/1000
    /// (e.g. 990 = p99), `None` when empty; `u64::MAX` in the overflow bucket.
    #[must_use]
    pub fn quantile_upper_ns(&self, permille: u64) -> Option<u64> {
        let total: u64 = self.buckets.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = total
            .saturating_mul(permille.min(1000))
            .div_ceil(1000)
            .max(1);
        let mut seen = 0u64;
        for (index, count) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return Some(LATENESS_EDGES_NS.get(index).copied().unwrap_or(u64::MAX));
            }
        }
        Some(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_split_at_the_documented_edges() {
        let h = TickLateness::default();
        h.record(100, 200); // early → 0
        h.record(500_000, 0); // exactly 0.5 ms → bucket 0
        h.record(500_001, 0); // → bucket 1
        h.record(16_700_000, 0); // 16.7 ms → bucket 5
        h.record(70_000_000, 0); // overflow
        let s = h.snapshot();
        assert_eq!(s.buckets, [2, 1, 0, 0, 0, 1, 0, 0, 1]);
        assert_eq!(s.count, 5);
        assert_eq!(s.max_ns, 70_000_000);
        assert_eq!(s.quantile_upper_ns(500), Some(1_000_000));
        assert_eq!(s.quantile_upper_ns(990), Some(u64::MAX));
        let w = s.since(&s);
        assert_eq!(w.count, 0);
        assert_eq!(w.quantile_upper_ns(500), None);
    }
}
