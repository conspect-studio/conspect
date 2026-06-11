//! Property tests for histogram bucketing invariants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::metrics::{Labels, MetricsRegistry};
use proptest::prelude::*;

proptest! {
    /// For any finite observations and any (finite) bound spec, the bucket
    /// accounting is exact: the per-bucket counts plus the overflow equal the
    /// total count, the cumulative counts are monotonically non-decreasing and
    /// end at the total count, and the +Inf count equals the total count.
    #[test]
    fn histogram_bucket_accounting_is_consistent(
        bounds in prop::collection::vec(-1000.0_f64..1000.0, 0..8),
        observations in prop::collection::vec(-2000.0_f64..2000.0, 0..200),
    ) {
        let reg = MetricsRegistry::new();
        let h = reg.histogram("prop_hist", Labels::empty(), &bounds);
        for &o in &observations {
            h.observe(o);
        }
        let snap = h.snapshot();

        // Total count equals the number of observations.
        prop_assert_eq!(snap.count, u64::try_from(observations.len()).unwrap());

        // Per-bucket counts + overflow == total count.
        let bucketed: u64 = snap.counts.iter().copied().sum::<u64>() + snap.overflow;
        prop_assert_eq!(bucketed, snap.count);

        // The +Inf bucket equals the total count.
        prop_assert_eq!(snap.inf_count(), snap.count);

        // Cumulative counts are monotonically non-decreasing.
        let cumulative = snap.cumulative_counts();
        for win in cumulative.windows(2) {
            prop_assert!(win[1] >= win[0]);
        }

        // The last cumulative count (if any finite buckets) never exceeds total.
        if let Some(&last) = cumulative.last() {
            prop_assert!(last <= snap.count);
            // It equals (count - overflow): everything except the +Inf overflow.
            prop_assert_eq!(last, snap.count - snap.overflow);
        }

        // An observation <= the smallest bound must land at-or-before that bound.
        // Concretely: every observation is counted exactly once across buckets.
    }

    /// Each observation falls in the lowest bucket whose bound is >= the value
    /// (Prometheus `le` semantics): for sorted finite bounds, the count of
    /// observations <= bound[i] equals cumulative[i].
    #[test]
    fn histogram_le_semantics_hold(
        observations in prop::collection::vec(0.0_f64..100.0, 0..150),
    ) {
        let bounds = [10.0_f64, 25.0, 50.0, 75.0];
        let reg = MetricsRegistry::new();
        let h = reg.histogram("le_hist", Labels::empty(), &bounds);
        for &o in &observations {
            h.observe(o);
        }
        let cumulative = h.snapshot().cumulative_counts();
        for (i, &bound) in bounds.iter().enumerate() {
            let matching = observations.iter().filter(|&&o| o <= bound).count();
            let expected = u64::try_from(matching).unwrap();
            prop_assert_eq!(cumulative[i], expected, "bucket {} (le {})", i, bound);
        }
    }
}
