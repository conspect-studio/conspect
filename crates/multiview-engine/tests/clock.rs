//! Output-clock tests — invariant #1 (the heart of the product).
//!
//! These prove `out_pts = f(tick)`: presentation timestamps are a pure function
//! of the integer tick counter and the fixed cadence, exact for every tick, with
//! ZERO drift over a million-plus ticks, strictly monotonic, and produced
//! independent of any input (there are no inputs in these tests — the clock is a
//! counter).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::{ManualTimeSource, MonotonicTimeSource, TimeSource};
use multiview_engine::OutputClock;
use proptest::prelude::*;

/// A **truly independent** oracle for `out_pts = f(tick)`, computed WITHOUT
/// touching `rescale`/`from_tick` (the code under test) so it cannot be a
/// tautology.
///
/// `out_pts(tick) = tick / fps` seconds in nanoseconds
///                = tick * `1_000_000_000` * `fps_den` / `fps_num`,
/// rounded **half away from zero**. Everything is done by hand in `i128` with an
/// explicit division + remainder so the rounding rule is visible and matches the
/// documented contract of [`multiview_core::time::rescale`] — derived from first
/// principles, not from the implementation.
fn oracle_pts_ns(tick: i64, cadence: Rational) -> i64 {
    // num = tick * 1e9 * fps_den ; den = fps_num. (fps_num > 0 by construction.)
    let numerator: i128 = i128::from(tick) * 1_000_000_000_i128 * i128::from(cadence.den);
    let denominator: i128 = i128::from(cadence.num);
    let q = numerator / denominator;
    let r = numerator % denominator;
    // Round half away from zero: |r| * 2 >= den rounds the quotient away from 0.
    let rounded = if r >= 0 {
        if r * 2 >= denominator {
            q + 1
        } else {
            q
        }
    } else if (-r) * 2 >= denominator {
        q - 1
    } else {
        q
    };
    i64::try_from(rounded).expect("oracle pts fits in i64 for the tested tick range")
}

#[test]
fn rejects_degenerate_cadence() {
    assert!(OutputClock::new(Rational::new(0, 1)).is_err());
    assert!(OutputClock::new(Rational::new(60, 0)).is_err());
    assert!(OutputClock::new(Rational::new(-30, 1)).is_err());
    assert!(OutputClock::new(Rational::new(30, -1)).is_err());
    // A valid NTSC-family cadence is accepted.
    assert!(OutputClock::new(Rational::FPS_59_94).is_ok());
}

#[test]
fn pts_equals_reference_for_every_tick() {
    let cadence = Rational::FPS_59_94; // 60000/1001 — the drift-prone case.
    let mut clock = OutputClock::new(cadence).unwrap();
    for expected_index in 0..10_000_u64 {
        let tick = clock.tick();
        assert_eq!(tick.index, expected_index, "tick index must be the counter");
        let i64_index = i64::try_from(expected_index).unwrap();
        assert_eq!(
            tick.pts.as_nanos(),
            oracle_pts_ns(i64_index, cadence),
            "out_pts must equal the independent i128 oracle exactly at tick {expected_index}"
        );
    }
}

#[test]
fn pts_equals_independent_oracle_across_cadences_and_wide_tick_range() {
    // The four canonical cadences the task pins, including both NTSC `1001`
    // rationals, checked against the INDEPENDENT i128 oracle across a wide tick
    // range (boundaries + a dense low range + far-out ticks). If the clock and
    // the oracle — derived from completely different code — agree everywhere,
    // the closed-form `out_pts = f(tick)` is correct.
    for cadence in [
        Rational::FPS_25,     // 25/1
        Rational::FPS_29_97,  // 30000/1001
        Rational::FPS_60,     // 60/1
        Rational::FPS_23_976, // 24000/1001
    ] {
        let clock = OutputClock::new(cadence).unwrap();
        // Dense low range to catch off-by-one rounding, plus far-out ticks to
        // catch any accumulation (there must be none — it is a pure function).
        let dense = 0..2_000_u64;
        let sparse = [
            10_000_u64, 999_999, 1_000_000, 1_234_567, 5_000_000, 10_000_000, 86_400_000,
        ];
        for index in dense.chain(sparse) {
            let i64_index = i64::try_from(index).unwrap();
            assert_eq!(
                clock.pts_at(index).as_nanos(),
                oracle_pts_ns(i64_index, cadence),
                "cadence {}/{} tick {index}: clock must equal the independent oracle",
                cadence.num,
                cadence.den
            );
        }
    }
}

#[test]
fn pts_is_strictly_monotonic() {
    for cadence in [
        Rational::FPS_25,
        Rational::FPS_30,
        Rational::FPS_29_97,
        Rational::FPS_60,
        Rational::FPS_59_94,
        Rational::FPS_23_976,
    ] {
        let mut clock = OutputClock::new(cadence).unwrap();
        let mut prev = i64::MIN;
        for _ in 0..5_000 {
            let pts = clock.tick().pts.as_nanos();
            assert!(
                pts > prev,
                "pts must strictly increase (cadence {}/{}): {pts} !> {prev}",
                cadence.num,
                cadence.den
            );
            prev = pts;
        }
    }
}

#[test]
fn zero_drift_over_a_million_ticks() {
    // The long-run property: after >= 1,000,000 ticks the pts is STILL exactly
    // the closed-form value, with no accumulated error. This is the test a
    // float-accumulating clock fails (29.97 drifts ~3.6 s/hour).
    let cadence = Rational::FPS_29_97; // 30000/1001.
    let clock = OutputClock::new(cadence).unwrap();
    let total: u64 = 1_000_000;

    // Compare the clock's pure pts_at against the closed form across the whole
    // span, and verify the endpoint matches the exact seconds-elapsed value.
    for index in [0_u64, 1, 999, 250_000, 500_000, 999_999, total] {
        let i64_index = i64::try_from(index).unwrap();
        assert_eq!(
            clock.pts_at(index).as_nanos(),
            oracle_pts_ns(i64_index, cadence),
            "no drift permitted at tick {index}"
        );
    }

    // Endpoint sanity: 1_000_000 frames at 30000/1001 fps == exactly
    // 1_000_000 * 1001 / 30000 seconds. Compare in nanoseconds, exact.
    let expected_ns = {
        // 1_000_000 * 1001 * 1_000_000_000 / 30_000, all in i128, exact.
        let n: i128 = 1_000_000_i128 * 1001 * 1_000_000_000;
        let q = n / 30_000;
        let r = n % 30_000;
        // round half away from zero
        if r * 2 >= 30_000 {
            q + 1
        } else {
            q
        }
    };
    assert_eq!(
        i128::from(clock.pts_at(total).as_nanos()),
        expected_ns,
        "endpoint pts after 1e6 ticks must be the exact closed-form value"
    );
}

#[test]
fn tick_produced_even_when_no_inputs_exist() {
    // There are no inputs anywhere in this crate's clock. Proving the clock
    // ticks "even when all inputs are absent" is proving it depends on NOTHING
    // but its counter: a fresh clock with zero collaborators still produces a
    // valid, monotonic, correctly-stamped tick every call.
    let mut clock = OutputClock::new(Rational::FPS_60).unwrap();
    let mut last = None;
    for expected in 0..1000_u64 {
        let tick = clock.tick();
        assert_eq!(tick.index, expected);
        // pts is valid (non-negative, finite-by-type) and increasing.
        assert!(tick.pts.as_nanos() >= 0);
        if let Some(prev) = last {
            assert!(tick.pts > prev);
        }
        last = Some(tick.pts);
    }
}

#[test]
fn deadline_is_seed_plus_pts() {
    let clock = OutputClock::new(Rational::FPS_50).unwrap();
    let seed = 1_000_000_000_i64; // arbitrary seed instant (1s on the source).
    for index in [0_u64, 1, 50, 123] {
        assert_eq!(
            clock.deadline_nanos(index, seed),
            seed + clock.pts_at(index).as_nanos(),
            "deadline must be seed + pts_at(index)"
        );
    }
    // 50 ticks at 50fps == exactly 1 second later than seed.
    assert_eq!(clock.deadline_nanos(50, seed), seed + 1_000_000_000);
}

#[test]
fn manual_time_source_is_monotonic_and_injectable() {
    let src = ManualTimeSource::new();
    assert_eq!(src.now_nanos(), 0);
    src.advance(std::time::Duration::from_millis(16));
    assert_eq!(src.now_nanos(), 16_000_000);
    // Setting backwards is clamped (monotonic guarantee).
    src.set(1_000_000);
    assert_eq!(src.now_nanos(), 16_000_000);
    src.set(20_000_000);
    assert_eq!(src.now_nanos(), 20_000_000);
}

#[test]
fn monotonic_time_source_never_decreases() {
    let src = MonotonicTimeSource::new();
    let a = src.now_nanos();
    let b = src.now_nanos();
    assert!(b >= a, "real monotonic source must not go backwards");
}

#[test]
fn time_source_is_object_safe_for_injection() {
    // The clock driver injects a `dyn TimeSource`; prove the trait is usable as
    // a trait object (the whole point of the injection seam).
    let src: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
    assert_eq!(src.now_nanos(), 0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For ANY positive cadence and ANY tick index, the clock's pts equals the
    /// **independent** i128 oracle (`round(tick * 1e9 * den / num)`, half away
    /// from zero, computed without calling `rescale`/`from_tick`) — the
    /// universally-quantified `out_pts = f(tick)`. This is the assertion that
    /// actually proves the clock's arithmetic is correct; comparing against a
    /// copy of the implementation would prove nothing.
    #[test]
    fn prop_pts_equals_independent_oracle(
        num in 1_i64..=120_000,
        den in 1_i64..=1001,
        index in 0_u64..2_000_000,
    ) {
        let cadence = Rational::new(num, den);
        let clock = OutputClock::new(cadence).unwrap();
        let i64_index = i64::try_from(index).unwrap();
        prop_assert_eq!(
            clock.pts_at(index).as_nanos(),
            oracle_pts_ns(i64_index, cadence)
        );
    }

    /// For ANY positive cadence, consecutive ticks are strictly increasing.
    #[test]
    fn prop_strictly_monotonic(
        num in 1_i64..=120_000,
        den in 1_i64..=1001,
        start in 0_u64..1_000_000,
    ) {
        let cadence = Rational::new(num, den);
        let clock = OutputClock::new(cadence).unwrap();
        let a = clock.pts_at(start);
        let b = clock.pts_at(start + 1);
        prop_assert!(b > a, "pts_at({}) !> pts_at({})", start + 1, start);
        // round-trip: the pts maps back to its tick (within one tick).
        let back = MediaTime::from_nanos(a.as_nanos()).to_tick(cadence);
        prop_assert!((back - i64::try_from(start).unwrap()).abs() <= 1);
    }
}
