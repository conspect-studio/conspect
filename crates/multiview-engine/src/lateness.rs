//! Program tick **lateness** (ADR-0108 §10, the Program protection bound):
//! emit time minus the tick's deadline, measured where the runtime publishes a
//! fresh or repeated tick.
//!
//! Wait-free on the clock (invariant #1): recording is a handful of
//! `fetch_add`/`fetch_max` on fixed atomics — no allocation, no lock, no
//! syscall beyond the time source read the loop already makes. Readers on
//! other threads take a [`LatenessSnapshot`] (cumulative counters); windowed
//! percentiles are the difference of two snapshots.
//!
//! Two bucket sets are reported (#311). The **coarse** set
//! ([`LATENESS_EDGES_NS`], 0.5 ms .. 66.8 ms) is the original ADR-0108 §10
//! histogram, kept byte-for-byte so existing readers do not change. The
//! **fine** set ([`FINE_LATENESS_EDGES_NS`]) adds 0.05, 0.1 and 0.25 ms edges
//! below 0.5 ms so a sub-millisecond improvement of the clock thread is
//! measurable. Only the fine buckets are stored; the coarse buckets are
//! derived from them at snapshot time (every coarse edge is also a fine edge),
//! so recording still touches exactly one bucket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Upper bucket edges in nanoseconds: 0.5, 1, 2, 4, 8, 16.7, 33.4, 66.8 ms;
/// a final overflow bucket holds everything later.
pub const LATENESS_EDGES_NS: [u64; 8] = [
    500_000, 1_000_000, 2_000_000, 4_000_000, 8_000_000, 16_700_000, 33_400_000, 66_800_000,
];
/// Buckets: one per edge plus the overflow.
pub const LATENESS_BUCKETS: usize = LATENESS_EDGES_NS.len() + 1;

/// Fine upper bucket edges in nanoseconds: 0.05, 0.1, 0.25, then every
/// coarse edge (0.5 .. 66.8 ms); a final overflow bucket follows.
pub const FINE_LATENESS_EDGES_NS: [u64; 11] = [
    50_000, 100_000, 250_000, 500_000, 1_000_000, 2_000_000, 4_000_000, 8_000_000, 16_700_000,
    33_400_000, 66_800_000,
];
/// Fine buckets: one per fine edge plus the overflow.
pub const FINE_LATENESS_BUCKETS: usize = FINE_LATENESS_EDGES_NS.len() + 1;

/// How many leading fine buckets merge into the first coarse bucket (the fine
/// edges below and at 0.5 ms). Every later fine bucket maps 1:1.
const FINE_BELOW_FIRST_COARSE: usize = FINE_LATENESS_BUCKETS - LATENESS_BUCKETS + 1;

/// The shared wait-free histogram.
#[derive(Debug, Default)]
pub struct TickLateness {
    fine: [AtomicU64; FINE_LATENESS_BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

/// A point-in-time copy of [`TickLateness`] (cumulative).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatenessSnapshot {
    /// Per-bucket counts (edges [`LATENESS_EDGES_NS`], then overflow).
    pub buckets: [u64; LATENESS_BUCKETS],
    /// Per-bucket counts on the fine edges ([`FINE_LATENESS_EDGES_NS`], then
    /// overflow). Sums to the same total as [`Self::buckets`].
    pub fine_buckets: [u64; FINE_LATENESS_BUCKETS],
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
        let index = FINE_LATENESS_EDGES_NS
            .iter()
            .position(|edge| late <= *edge)
            .unwrap_or(FINE_LATENESS_EDGES_NS.len());
        if let Some(bucket) = self.fine.get(index) {
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
        let mut fine_buckets = [0; FINE_LATENESS_BUCKETS];
        for (out, bucket) in fine_buckets.iter_mut().zip(&self.fine) {
            *out = bucket.load(Ordering::Relaxed);
        }
        LatenessSnapshot {
            buckets: coarse_of(&fine_buckets),
            fine_buckets,
            count,
            sum_ns: self.sum_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
        }
    }
}

/// Fold the fine buckets into the coarse ones: the fine buckets at and below
/// 0.5 ms sum into coarse bucket 0, every later fine bucket maps 1:1.
fn coarse_of(fine: &[u64; FINE_LATENESS_BUCKETS]) -> [u64; LATENESS_BUCKETS] {
    let mut coarse = [0u64; LATENESS_BUCKETS];
    for (index, count) in fine.iter().enumerate() {
        let target = index.saturating_sub(FINE_BELOW_FIRST_COARSE - 1);
        if let Some(slot) = coarse.get_mut(target) {
            *slot = slot.saturating_add(*count);
        }
    }
    coarse
}

/// The per-tick phase histograms of the Program loop (#311 reopen): where a
/// tick's emit lateness comes from. Each is a [`TickLateness`] recording a
/// duration (`record(end, start)`), so they share its fine buckets.
///
/// * `wake` — time right after the pacer's wait returns, minus the tick's
///   deadline (OS wake latency plus any CPU-starvation of the clock thread).
/// * `compose` — the whole fresh-tick body: the frame-boundary control hook
///   (`before_compose`) through `publish_state`. Emit lateness of a fresh
///   tick is roughly `wake + compose`.
/// * `hook`, `composite`, `after`, `project` — the four parts of `compose`:
///   `before_compose`, `CompositorDrive::compose`, `after_compose`, and the
///   state projection plus `publish_state`.
///
/// Recording is a time-source read plus a few relaxed atomics per phase — no
/// lock, no allocation (invariant #1).
#[derive(Debug, Default)]
pub struct TickPhases {
    /// Wake lateness: wait return minus deadline.
    pub wake: TickLateness,
    /// `before_compose` through `publish_state`.
    pub compose: TickLateness,
    /// `before_compose` (the frame-boundary control hook).
    pub hook: TickLateness,
    /// `CompositorDrive::compose` (sample + composite).
    pub composite: TickLateness,
    /// `after_compose` (switcher commit, preview tap, routing aux offer).
    pub after: TickLateness,
    /// The state projection (`state_of`) plus `publish_state`.
    pub project: TickLateness,
}

/// A point-in-time copy of [`TickPhases`] (cumulative).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhasesSnapshot {
    /// See [`TickPhases::wake`].
    pub wake: LatenessSnapshot,
    /// See [`TickPhases::compose`].
    pub compose: LatenessSnapshot,
    /// See [`TickPhases::hook`].
    pub hook: LatenessSnapshot,
    /// See [`TickPhases::composite`].
    pub composite: LatenessSnapshot,
    /// See [`TickPhases::after`].
    pub after: LatenessSnapshot,
    /// See [`TickPhases::project`].
    pub project: LatenessSnapshot,
}

impl TickPhases {
    /// A fresh shared set.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The cumulative counters now.
    #[must_use]
    pub fn snapshot(&self) -> PhasesSnapshot {
        PhasesSnapshot {
            wake: self.wake.snapshot(),
            compose: self.compose.snapshot(),
            hook: self.hook.snapshot(),
            composite: self.composite.snapshot(),
            after: self.after.snapshot(),
            project: self.project.snapshot(),
        }
    }
}

impl PhasesSnapshot {
    /// The counts recorded since `earlier` (a window).
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            wake: self.wake.since(&earlier.wake),
            compose: self.compose.since(&earlier.compose),
            hook: self.hook.since(&earlier.hook),
            composite: self.composite.since(&earlier.composite),
            after: self.after.since(&earlier.after),
            project: self.project.since(&earlier.project),
        }
    }
}

/// The upper edge (ns) of the bucket holding quantile `permille`/1000 over
/// `buckets` with upper `edges` (the last bucket is the overflow).
fn quantile_over(buckets: &[u64], edges: &[u64], permille: u64) -> Option<u64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = total
        .saturating_mul(permille.min(1000))
        .div_ceil(1000)
        .max(1);
    let mut seen = 0u64;
    for (index, count) in buckets.iter().enumerate() {
        seen = seen.saturating_add(*count);
        if seen >= rank {
            return Some(edges.get(index).copied().unwrap_or(u64::MAX));
        }
    }
    Some(u64::MAX)
}

impl LatenessSnapshot {
    /// The counts recorded since `earlier` (a window).
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        let mut buckets = [0; LATENESS_BUCKETS];
        for ((out, now), then) in buckets.iter_mut().zip(&self.buckets).zip(&earlier.buckets) {
            *out = now.saturating_sub(*then);
        }
        let mut fine_buckets = [0; FINE_LATENESS_BUCKETS];
        for ((out, now), then) in fine_buckets
            .iter_mut()
            .zip(&self.fine_buckets)
            .zip(&earlier.fine_buckets)
        {
            *out = now.saturating_sub(*then);
        }
        Self {
            buckets,
            fine_buckets,
            count: self.count.saturating_sub(earlier.count),
            sum_ns: self.sum_ns.saturating_sub(earlier.sum_ns),
            max_ns: self.max_ns,
        }
    }

    /// The upper edge (ns) of the coarse bucket holding the quantile
    /// `permille`/1000 (e.g. 990 = p99), `None` when empty; `u64::MAX` in the
    /// overflow bucket.
    #[must_use]
    pub fn quantile_upper_ns(&self, permille: u64) -> Option<u64> {
        quantile_over(&self.buckets, &LATENESS_EDGES_NS, permille)
    }

    /// Like [`Self::quantile_upper_ns`] on the fine edges
    /// ([`FINE_LATENESS_EDGES_NS`]).
    #[must_use]
    pub fn fine_quantile_upper_ns(&self, permille: u64) -> Option<u64> {
        quantile_over(&self.fine_buckets, &FINE_LATENESS_EDGES_NS, permille)
    }

    /// Mean lateness in nanoseconds (`sum_ns / count`), `None` when empty.
    #[must_use]
    pub fn mean_ns(&self) -> Option<u64> {
        self.sum_ns.checked_div(self.count)
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

    #[test]
    fn fine_buckets_resolve_sub_half_millisecond_lateness() {
        let h = TickLateness::default();
        h.record(0, 0); // 0 → fine 0 (≤ 0.05 ms)
        h.record(50_000, 0); // exactly 0.05 ms → fine 0
        h.record(50_001, 0); // → fine 1 (≤ 0.1 ms)
        h.record(100_001, 0); // → fine 2 (≤ 0.25 ms)
        h.record(250_001, 0); // → fine 3 (≤ 0.5 ms)
        h.record(1_500_000, 0); // → fine 5 (≤ 2 ms)
        h.record(70_000_000, 0); // overflow
        let s = h.snapshot();
        assert_eq!(s.fine_buckets, [2, 1, 1, 1, 0, 1, 0, 0, 0, 0, 0, 1]);
        // The coarse view is unchanged: everything ≤ 0.5 ms is coarse bucket 0.
        assert_eq!(s.buckets, [5, 0, 1, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            s.fine_buckets.iter().sum::<u64>(),
            s.buckets.iter().sum::<u64>()
        );
        assert_eq!(s.fine_quantile_upper_ns(500), Some(250_000));
        assert_eq!(s.quantile_upper_ns(500), Some(500_000));
        assert_eq!(
            s.mean_ns(),
            Some((50_000 + 50_001 + 100_001 + 250_001 + 1_500_000 + 70_000_000) / 7)
        );
        let w = s.since(&s);
        assert_eq!(w.mean_ns(), None);
        assert_eq!(w.fine_quantile_upper_ns(990), None);
    }

    #[test]
    fn every_coarse_edge_is_a_fine_edge() {
        for edge in LATENESS_EDGES_NS {
            assert!(FINE_LATENESS_EDGES_NS.contains(&edge), "{edge}");
        }
        assert_eq!(
            FINE_LATENESS_EDGES_NS
                .get(FINE_BELOW_FIRST_COARSE - 1)
                .copied(),
            LATENESS_EDGES_NS.first().copied()
        );
    }
}
