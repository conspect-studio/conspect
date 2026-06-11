//! Integration tests for the `time` module: exact rational math and rescaling.
//!
//! These pin invariant #3 (unified timing model, "never float fps"): all
//! conversions are exact integer math via `i128` intermediates.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{rescale, MediaTime, Rational};
use proptest::prelude::*;

#[test]
fn rational_reduce_normalizes_sign_and_gcd() {
    // 6/-4 reduces to -3/2 (sign on numerator, positive denominator).
    let r = Rational::new(6, -4).reduce();
    assert_eq!(r.num, -3);
    assert_eq!(r.den, 2);

    // -6/-4 reduces to 3/2.
    let r = Rational::new(-6, -4).reduce();
    assert_eq!(r.num, 3);
    assert_eq!(r.den, 2);

    // Zero numerator reduces to 0/1.
    let r = Rational::new(0, 5).reduce();
    assert_eq!(r.num, 0);
    assert_eq!(r.den, 1);
}

#[test]
fn rational_ntsc_consts_are_exact_rationals() {
    // The 29.97 family MUST be 30000/1001 exactly, never a float.
    assert_eq!(Rational::FPS_29_97.num, 30000);
    assert_eq!(Rational::FPS_29_97.den, 1001);
    assert_eq!(Rational::FPS_59_94.num, 60000);
    assert_eq!(Rational::FPS_59_94.den, 1001);
    // It is already reduced (gcd(30000,1001)=1).
    assert_eq!(Rational::FPS_29_97.reduce(), Rational::FPS_29_97);
}

#[test]
fn rational_inv_swaps_and_normalizes() {
    let r = Rational::new(30000, 1001);
    let inv = r.inv().unwrap();
    assert_eq!(inv.num, 1001);
    assert_eq!(inv.den, 30000);

    // inv of a negative keeps the sign on the numerator.
    let r = Rational::new(-3, 2);
    let inv = r.inv().unwrap();
    assert_eq!(inv.num, -2);
    assert_eq!(inv.den, 3);

    // inv of zero is None (no division by zero).
    assert!(Rational::new(0, 7).inv().is_none());
}

#[test]
fn rational_checked_mul_reduces_result() {
    // (2/3) * (3/4) = 6/12 = 1/2, returned reduced.
    let p = Rational::new(2, 3)
        .checked_mul(Rational::new(3, 4))
        .unwrap();
    assert_eq!(p.num, 1);
    assert_eq!(p.den, 2);
}

#[test]
fn rational_checked_mul_detects_overflow() {
    // Two huge numerators overflow i64 even after gcd reduction.
    let big = Rational::new(i64::MAX, 1);
    assert!(big.checked_mul(big).is_none());
}

#[test]
fn rational_ord_uses_cross_multiplication_without_overflow() {
    // 30000/1001 (~29.97) < 60000/1001 (~59.94).
    assert!(Rational::FPS_29_97 < Rational::FPS_59_94);
    // 1/3 < 1/2 even though numerators are equal.
    assert!(Rational::new(1, 3) < Rational::new(1, 2));
    // Equal value, different representation, compare Equal.
    assert_eq!(
        Rational::new(2, 4).cmp(&Rational::new(1, 2)),
        std::cmp::Ordering::Equal
    );
    // Negative handling: -1/2 < 1/3.
    assert!(Rational::new(-1, 2) < Rational::new(1, 3));
    // Cross-multiplying these as i64 would overflow; i128 must not.
    let a = Rational::new(i64::MAX, 2);
    let b = Rational::new(i64::MAX, 3);
    assert!(b < a);
}

#[test]
fn rational_is_zero() {
    assert!(Rational::new(0, 9).is_zero());
    assert!(!Rational::new(1, 9).is_zero());
}

#[test]
fn rescale_exact_for_commensurate_timebases() {
    // 90 kHz ticks -> nanoseconds: 1 tick @ 1/90000 == 11111ns rounded.
    // Use a clean example: 90000 ticks @ 1/90000 s = 1 s = 1_000_000_000 ns.
    let ns = rescale(
        90_000,
        Rational::new(1, 90_000),
        Rational::new(1, 1_000_000_000),
    );
    assert_eq!(ns, 1_000_000_000);
}

#[test]
fn rescale_rounds_to_nearest() {
    // 1 tick @ 1/3 s expressed in whole seconds (1/1) rounds 0.333 -> 0.
    assert_eq!(rescale(1, Rational::new(1, 3), Rational::new(1, 1)), 0);
    // 2 ticks @ 1/3 s -> 0.667 -> 1.
    assert_eq!(rescale(2, Rational::new(1, 3), Rational::new(1, 1)), 1);
    // Exactly .5 rounds away from zero (round-half-up magnitude).
    assert_eq!(rescale(1, Rational::new(1, 2), Rational::new(1, 1)), 1);
    assert_eq!(rescale(-1, Rational::new(1, 2), Rational::new(1, 1)), -1);
}

#[test]
fn mediatime_from_tick_is_exact_for_ntsc() {
    // tick 1001 @ 30000/1001 fps == 1001/30000 s ... but the helper takes a
    // cadence (fps). out_pts(tick) = tick / fps. At 30000/1001 fps, one full
    // 30000-tick block is exactly 1001 seconds? No: tick/fps = tick * 1001/30000 s.
    // tick = 30000 -> 1001 s? 30000 * 1001/30000 = 1001 s. Verify in ns.
    let t = MediaTime::from_tick(30_000, Rational::FPS_29_97);
    assert_eq!(t.as_nanos(), 1001 * 1_000_000_000);
}

#[test]
fn mediatime_round_trips_ticks_within_one_unit() {
    let cadence = Rational::FPS_59_94;
    for tick in [0_i64, 1, 2, 59, 60, 1000, 60_000] {
        let t = MediaTime::from_tick(tick, cadence);
        let back = t.to_tick(cadence);
        assert!(
            (back - tick).abs() <= 1,
            "tick {tick} round-tripped to {back}"
        );
    }
}

#[test]
fn mediatime_saturating_add_sub() {
    let a = MediaTime::from_nanos(i64::MAX - 1);
    assert_eq!(
        a.saturating_add(MediaTime::from_nanos(10)).as_nanos(),
        i64::MAX
    );
    let b = MediaTime::from_nanos(i64::MIN + 1);
    assert_eq!(
        b.saturating_sub(MediaTime::from_nanos(10)).as_nanos(),
        i64::MIN
    );
    // Ordinary case.
    assert_eq!(
        MediaTime::from_nanos(100)
            .saturating_sub(MediaTime::from_nanos(40))
            .as_nanos(),
        60
    );
}

proptest! {
    /// `reduce()` always yields a canonical form: positive denominator and a
    /// reduced fraction, and is idempotent.
    #[test]
    fn prop_reduce_is_canonical_and_idempotent(num in -1_000_000_000_i64..=1_000_000_000, den in -1_000_000_000_i64..=1_000_000_000) {
        prop_assume!(den != 0);
        let r = Rational::new(num, den).reduce();
        prop_assert!(r.den > 0);
        // Idempotent.
        prop_assert_eq!(r.reduce(), r);
        // Value preserved: num/den == r.num/r.den  <=>  num*r.den == r.num*den (i128).
        let lhs = i128::from(num) * i128::from(r.den);
        let rhs = i128::from(r.num) * i128::from(den);
        prop_assert_eq!(lhs, rhs);
    }

    /// `rescale` never panics and stays within `i64` for any `i64` value and
    /// any positive timebases.
    #[test]
    fn prop_rescale_never_panics(value in any::<i64>(), fn1 in 1_i64..=1_000_000, fd1 in 1_i64..=1_000_000, fn2 in 1_i64..=1_000_000, fd2 in 1_i64..=1_000_000) {
        let from = Rational::new(fn1, fd1);
        let to = Rational::new(fn2, fd2);
        let _ = rescale(value, from, to); // must not panic/overflow.
    }

    /// Rescaling to the same timebase is the identity.
    #[test]
    fn prop_rescale_identity(value in any::<i64>(), n in 1_i64..=1_000_000, d in 1_i64..=1_000_000) {
        let tb = Rational::new(n, d);
        prop_assert_eq!(rescale(value, tb, tb), value);
    }

    /// Round-trip through a finer then back to the original timebase stays
    /// within 1 unit (round-to-nearest can lose at most half a unit each way).
    #[test]
    fn prop_rescale_round_trip_within_one(value in -1_000_000_i64..=1_000_000, n in 1_i64..=1000, d in 1_i64..=1000) {
        let coarse = Rational::new(n, d);
        // nanoseconds-ish fine timebase
        let fine = Rational::new(1, 1_000_000_000);
        let to_fine = rescale(value, coarse, fine);
        let back = rescale(to_fine, fine, coarse);
        prop_assert!((back - value).abs() <= 1, "value {} -> {} -> {}", value, to_fine, back);
    }
}
