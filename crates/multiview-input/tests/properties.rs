//! Property tests for the input timing/resilience logic.
//!
//! These pin the load-bearing invariants:
//!   * wrap-unwrap is monotonic across the 33-bit boundary;
//!   * the normalizer NEVER emits a backwards ns timestamp;
//!   * the pacer releases in PTS order at the right wall-clock;
//!   * the bounded reorder buffer never exceeds capacity.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{MediaTime, Rational};
use multiview_input::jitter::ReorderBuffer;
use multiview_input::normalize::{PtsNormalizer, WrapBits};
use multiview_input::pacer::Pacer;
use proptest::prelude::*;

fn ts_tb() -> Rational {
    Rational::new(1, 90_000)
}

proptest! {
    /// The normalizer NEVER emits a backwards ns timestamp, regardless of the
    /// raw PTS sequence (out-of-order, repeated, wrapping, or missing).
    #[test]
    fn prop_normalizer_is_strictly_monotonic(
        raws in prop::collection::vec(
            prop_oneof![
                Just(None),
                (0_i64..(1_i64 << 33)).prop_map(Some),
            ],
            1..200,
        ),
    ) {
        let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
        let mut last: Option<i64> = None;
        for raw in raws {
            let out = n.normalize(raw, 0).unwrap();
            if let Some(prev) = last {
                prop_assert!(
                    out.as_nanos() > prev,
                    "non-monotonic: {} !> {}",
                    out.as_nanos(),
                    prev
                );
            }
            last = Some(out.as_nanos());
        }
    }

    /// Wrap-unwrap across the 33-bit boundary recovers the true elapsed delta:
    /// stepping a fixed increment that crosses the wrap point still advances the
    /// output by that increment (in ns), not by a ~26.5h backwards jump.
    #[test]
    fn prop_wrap_unwrap_monotonic_across_boundary(
        // Start within one step of the wrap point; step is a small positive
        // tick increment (1..=90000 ticks = up to 1s @ 90kHz).
        step in 1_i64..=90_000,
        count in 2_usize..=50,
    ) {
        let wrap = 1_i64 << 33;
        let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
        // Begin just below the wrap so the sequence crosses it.
        let start = wrap - step; // first sample, then we cross
        let mut raw = start;
        let mut outs = Vec::with_capacity(count);
        for _ in 0..count {
            let masked = raw & (wrap - 1);
            outs.push(n.normalize(Some(masked), 0).unwrap().as_nanos());
            raw += step;
        }
        // Each per-step output delta must equal `step` ticks in ns to within 1ns
        // of nearest-rounding (each absolute rescale rounds independently), and
        // must be strictly positive — proving the wrap was unwrapped, NOT seen as
        // a ~26.5h backwards jump.
        let step_ns = multiview_core::time::rescale(
            step,
            ts_tb(),
            Rational::new(1, 1_000_000_000),
        );
        for w in outs.windows(2) {
            let delta = w[1] - w[0];
            prop_assert!(delta > 0, "delta must advance, got {}", delta);
            prop_assert!(
                (delta - step_ns).abs() <= 1,
                "per-step delta {} not ~{} (wrap not unwrapped?)",
                delta,
                step_ns
            );
        }
        // The TOTAL elapsed across all steps must equal the true elapsed
        // ((count-1)*step ticks) to within rounding — a missed wrap would make
        // this wildly negative (~ -2^33 ticks).
        let first = outs.first().copied().unwrap();
        let last = outs.last().copied().unwrap();
        let steps = i64::try_from(count - 1).unwrap();
        let total_true_ns = multiview_core::time::rescale(
            steps.saturating_mul(step),
            ts_tb(),
            Rational::new(1, 1_000_000_000),
        );
        prop_assert!(
            (last - first - total_true_ns).abs() <= steps + 1,
            "total elapsed {} not ~{}",
            last - first,
            total_true_ns
        );
    }

    /// The reorder buffer never exceeds its capacity, no matter the push order
    /// or count.
    #[test]
    fn prop_reorder_buffer_bounded(
        cap in 1_usize..=32,
        pushes in prop::collection::vec(any::<i32>(), 0..500),
    ) {
        let mut b: ReorderBuffer<i32> = ReorderBuffer::new(cap);
        for v in pushes {
            let ts = MediaTime::from_nanos(i64::from(v));
            b.push(ts, v);
            prop_assert!(b.len() <= cap, "len {} > cap {}", b.len(), cap);
        }
    }

    /// Anything drained from the reorder buffer comes out in non-decreasing PTS
    /// order (within a single drain pass with a fixed watermark).
    #[test]
    fn prop_reorder_drain_sorted(
        cap in 4_usize..=64,
        tss in prop::collection::vec(0_i64..1_000_000, 0..200),
    ) {
        let mut b: ReorderBuffer<i64> = ReorderBuffer::new(cap);
        for ts in tss {
            b.push(MediaTime::from_nanos(ts), ts);
        }
        let mut last = i64::MIN;
        // Drain the full buffer; each released ts must be >= the previous.
        while let Some((mt, _)) = b.pop() {
            prop_assert!(mt.as_nanos() >= last, "pop order not sorted");
            last = mt.as_nanos();
        }
    }

    /// The pacer's release deadline is exactly `anchor_wall + (pts - pts0)` and
    /// is monotonic in PTS: later PTS never schedules earlier (no
    /// discontinuity/jump in the input).
    #[test]
    fn prop_pacer_release_in_pts_order(
        anchor_wall in 0_i64..1_000_000_000_000,
        pts0 in 0_i64..1_000_000_000,
        // Strictly increasing, modest offsets (well under the discontinuity
        // threshold) so no re-anchoring fires.
        offsets in prop::collection::vec(1_i64..1_000_000_000, 1..50),
    ) {
        let mut p = Pacer::new(multiview_input::pacer::PacerConfig::default());
        // Anchor.
        let _ = p.submit(MediaTime::from_nanos(pts0), anchor_wall);
        let mut cum = 0_i64;
        let mut last_deadline = anchor_wall;
        for off in offsets {
            cum = cum.saturating_add(off);
            let pts = MediaTime::from_nanos(pts0.saturating_add(cum));
            let d = p.release_deadline(pts).unwrap();
            prop_assert_eq!(d, anchor_wall.saturating_add(cum));
            prop_assert!(d >= last_deadline, "deadlines must be non-decreasing");
            last_deadline = d;
        }
    }
}
