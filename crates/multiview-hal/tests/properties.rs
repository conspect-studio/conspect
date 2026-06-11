#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Property tests for the cost model and the hysteresis controller.

use multiview_core::time::Rational;
use multiview_hal::degradation::{Hysteresis, HysteresisConfig, LadderMove, MAX_LEVEL};
use multiview_hal::perf::{anchor, PerfClass, PerfSignals};
use multiview_hal::{CostBudget, Plan, Planner, Resolution, Stage, TileLoad};
use proptest::prelude::*;

proptest! {
    /// Tile load is monotone non-decreasing in pixel area at a fixed cadence:
    /// a bigger tile never costs less. (Invariant #6 budgets by megapixels.)
    #[test]
    fn load_monotone_in_area(
        w1 in 16u32..3840, h1 in 16u32..2160,
        w2 in 16u32..3840, h2 in 16u32..2160,
    ) {
        let cadence = Rational::FPS_30;
        let r1 = Resolution::new(w1, h1);
        let r2 = Resolution::new(w2, h2);
        let l1 = TileLoad::new(Stage::Decode, r1).megapixels_per_sec(cadence);
        let l2 = TileLoad::new(Stage::Decode, r2).megapixels_per_sec(cadence);
        if r1.pixels() <= r2.pixels() {
            prop_assert!(l1 <= l2 + 1e-9);
        } else {
            prop_assert!(l2 <= l1 + 1e-9);
        }
    }

    /// Load scales linearly with cadence: doubling the integer fps doubles the
    /// megapixels/sec.
    #[test]
    fn load_linear_in_fps(fps in 1i64..120, w in 16u32..3840, h in 16u32..2160) {
        let res = Resolution::new(w, h);
        let single = TileLoad::new(Stage::Encode, res)
            .megapixels_per_sec(Rational::new(fps, 1));
        let double = TileLoad::new(Stage::Encode, res)
            .megapixels_per_sec(Rational::new(fps * 2, 1));
        prop_assert!((double - 2.0 * single).abs() <= 1e-6 * double.max(1.0));
    }

    /// Admission accepts exactly when every stage's summed load is `<=` budget.
    /// The boolean result of `admit` must agree with a direct per-stage check.
    #[test]
    fn admission_agrees_with_direct_budget_check(
        budget in 0.0f64..500.0,
        counts in proptest::collection::vec(0u32..6, 3),
    ) {
        let cadence = Rational::FPS_30;
        let mut loads = Vec::new();
        for (idx, &count) in counts.iter().enumerate() {
            let stage = Stage::ALL[idx];
            for _ in 0..count {
                loads.push(TileLoad::new(stage, Resolution::HD720));
            }
        }
        let plan = Plan::new(cadence, loads);
        let planner = Planner::new(CostBudget::new(budget, budget, budget)).unwrap();

        let fits_all = Stage::ALL
            .iter()
            .all(|&s| plan.stage_load_mpps(s) <= budget);
        prop_assert_eq!(planner.admit(&plan).is_ok(), fits_all);
    }

    /// The hysteresis level always stays within `0..=MAX_LEVEL`, and each
    /// `observe` moves it by at most one rung, regardless of the (possibly
    /// adversarial / non-finite) pressure sequence. This is the structural
    /// anti-thrash guarantee.
    #[test]
    fn hysteresis_level_is_bounded_and_steps_by_at_most_one(
        pressures in proptest::collection::vec(-2.0f64..2.0, 1..200),
        cooldown in 0u32..8,
    ) {
        let cfg = HysteresisConfig::try_new(0.7, 0.9, cooldown).unwrap();
        let mut h = Hysteresis::new(cfg);
        let mut prev = h.level();
        for p in pressures {
            let mv = h.observe(p);
            let now = h.level();
            prop_assert!(now <= MAX_LEVEL);
            // The move and the level delta must be consistent.
            match mv {
                LadderMove::Down => prop_assert_eq!(now, prev + 1),
                LadderMove::Up => prop_assert_eq!(now + 1, prev),
                LadderMove::Hold => prop_assert_eq!(now, prev),
            }
            prev = now;
        }
    }

    /// Two consecutive recoveries (Up moves) can never be adjacent in time when
    /// the cooldown is positive: there must be at least `cooldown` ticks between
    /// recovery moves. This is the no-flap guarantee for the recovery direction.
    #[test]
    fn recoveries_are_separated_by_at_least_the_cooldown(
        cooldown in 1u32..6,
    ) {
        let cfg = HysteresisConfig::try_new(0.7, 0.9, cooldown).unwrap();
        let mut h = Hysteresis::new(cfg);
        // Climb to the top of the ladder.
        for _ in 0..MAX_LEVEL {
            prop_assert_eq!(h.observe(0.99), LadderMove::Down);
        }
        // Now feed sustained low pressure and record gaps between Up moves.
        let cooldown_ticks = usize::try_from(cooldown).unwrap();
        let mut last_up: Option<usize> = None;
        for tick in 0..(MAX_LEVEL * (cooldown_ticks + 2)) {
            if h.observe(0.0) == LadderMove::Up {
                if let Some(prev) = last_up {
                    prop_assert!(tick - prev >= cooldown_ticks);
                }
                last_up = Some(tick);
            }
        }
        // Eventually we fully recover.
        prop_assert_eq!(h.level(), 0);
    }

    /// Perf-class scaling is monotone in the cores x clock perf index: a device
    /// with a strictly-higher (cores x clock) product never gets a strictly-lower
    /// per-stage ceiling. (The inv-#1 ordering must never invert — a stronger GPU
    /// is always at least as trusted to sustain a stage.)
    #[test]
    fn perf_class_monotone_in_cores_times_clock(
        c1 in 1u32..20_000, k1 in 200u32..3_500,
        c2 in 1u32..20_000, k2 in 200u32..3_500,
    ) {
        let a = PerfClass::for_device(&PerfSignals::from_nvml(Some("X"), Some(c1), Some(k1), None));
        let b = PerfClass::for_device(&PerfSignals::from_nvml(Some("X"), Some(c2), Some(k2), None));
        let i1 = u64::from(c1) * u64::from(k1);
        let i2 = u64::from(c2) * u64::from(k2);
        let (lo, hi) = if i1 <= i2 { (a, b) } else { (b, a) };
        prop_assert!(hi.decode_mpps_ceiling() + 1e-6 >= lo.decode_mpps_ceiling());
        prop_assert!(hi.composite_mpps_ceiling() + 1e-6 >= lo.composite_mpps_ceiling());
        prop_assert!(hi.encode_mpps_ceiling() + 1e-6 >= lo.encode_mpps_ceiling());
        // And every ceiling is always finite (never infinite) regardless of input.
        prop_assert!(a.composite_mpps_ceiling().is_finite());
        prop_assert!(b.composite_mpps_ceiling().is_finite());
    }

    /// The anchor (RTX 4060) cores x clock reproduces its table ceilings: the two
    /// resolution paths agree at the calibration point.
    #[test]
    fn anchor_index_reproduces_table(noise in 0u32..2) {
        // `noise` only varies the case prop runs; the assertion is fixed.
        let _ = noise;
        let scaled = anchor::scaled(anchor::CORES, anchor::CLOCK_MHZ);
        prop_assert_eq!(scaled, anchor::class());
    }
}
