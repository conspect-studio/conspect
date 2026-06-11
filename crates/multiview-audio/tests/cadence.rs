// SampleClock — the per-output-tick audio sample budget (AUD-3). At a fixed
// output cadence `num/den` fps and a sample rate, each tick must emit an EXACT
// integer number of samples whose long-run total is a pure function of the tick
// count (the audio analogue of invariant #1's "exactly N frames for N ticks").
// For 1001-denominator (NTSC) rates the per-tick count is fractional
// (30000/1001 @ 48 kHz = 1601.6), so the remainder must accumulate across ticks
// (1601/1602 alternation) with NO float truncation — long-run drift would be the
// audio version of the float-fps drift the timing invariants forbid.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_audio::cadence::SampleClock;
use multiview_core::time::Rational;

#[test]
fn integer_rate_emits_a_constant_exact_budget() {
    // 25 fps @ 48 kHz = exactly 1920 samples/tick, forever, no remainder.
    let mut sc = SampleClock::new(48_000, Rational::new(25, 1));
    for _ in 0..200 {
        assert_eq!(sc.next_tick(), 1920);
    }
    // 30 fps @ 48 kHz = exactly 1600/tick.
    let mut sc30 = SampleClock::new(48_000, Rational::new(30, 1));
    let total: usize = (0..1000).map(|_| sc30.next_tick()).sum();
    assert_eq!(total, 1_600_000);
}

#[test]
fn ntsc_rate_alternates_and_stays_exact_over_the_long_run() {
    // 30000/1001 fps @ 48 kHz = 1601.6 samples/tick. Each tick is 1601 or 1602,
    // and over 5 ticks the total is exactly 8008 (= 5 * 1601.6).
    let mut sc = SampleClock::new(48_000, Rational::new(30_000, 1001));
    let first5: Vec<usize> = (0..5).map(|_| sc.next_tick()).collect();
    assert!(
        first5.iter().all(|&s| s == 1601 || s == 1602),
        "each NTSC tick is 1601 or 1602, got {first5:?}"
    );
    assert_eq!(
        first5.iter().sum::<usize>(),
        8008,
        "5 ticks = 5*1601.6 = 8008"
    );

    // Continuity over a long run: 30000 ticks at 30000/1001 fps spans exactly
    // 1001 seconds, i.e. exactly 1001 * 48000 = 48_048_000 samples — the long-run
    // count is an EXACT function of the tick count (no accumulated drift).
    let mut sc2 = SampleClock::new(48_000, Rational::new(30_000, 1001));
    let total: usize = (0..30_000).map(|_| sc2.next_tick()).sum();
    assert_eq!(total, 48_048_000);
}

#[test]
fn budget_never_drifts_off_the_running_average() {
    // Stronger continuity: after t ticks the cumulative samples must equal
    // floor(t * rate * den / num) for every t — i.e. it tracks the ideal real
    // sample position to within one sample at all times (gap-free, never ahead).
    let (rate, num, den) = (48_000_u64, 30_000_u64, 1001_u64);
    let mut sc = SampleClock::new(48_000, Rational::new(30_000, 1001));
    let mut cum: u64 = 0;
    for t in 1..=2000_u64 {
        cum += u64::try_from(sc.next_tick()).unwrap();
        let ideal = (t * rate * den) / num; // floor of the exact position
        assert_eq!(
            cum, ideal,
            "cumulative samples must track the ideal at tick {t}"
        );
    }
}
