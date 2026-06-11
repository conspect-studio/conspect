//! Invariant #1 for the PTP reference tracker: the PTP discipline **informs** the
//! wall-clock badge but must **never pace or stall the output clock**.
//!
//! The output clock stays the single fixed-cadence monotonic source:
//! `out_pts = f(tick)`. This test proves the `OutputClock` produces the exact
//! same tick count and the exact same PTS for every tick whether or not a
//! `ReferenceTracker` is concurrently fed wild, jittering, or entirely absent
//! samples. The tracker owns no clock and exposes no method that the output
//! clock calls, so it is *structurally* incapable of pacing — this test pins
//! that behaviourally so a future change that wired PTP into pacing would fail.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{MediaTime, Rational};
use multiview_engine::ptp::{LockState, PtpSample, ReferenceConfig, ReferenceTracker, ServoConfig};
use multiview_engine::OutputClock;

fn cfg() -> ReferenceConfig {
    ReferenceConfig {
        servo: ServoConfig::new_default(),
        lock_tolerance_ns: 50_000,
        lock_samples: 3,
        stale_after_ns: 2_000_000_000,
        holdover_window_ns: 5_000_000_000,
    }
}

/// Run an output clock for `n` ticks, optionally feeding a reference tracker a
/// pathological sample on each tick, and collect the PTS of every tick.
fn run(n: i64, feed_tracker: bool) -> Vec<MediaTime> {
    let mut clock = OutputClock::new(Rational::FPS_59_94).expect("valid cadence");
    let mut tracker = ReferenceTracker::new(cfg());
    let mut out = Vec::new();
    for i in 0..n {
        if feed_tracker {
            // Deliberately pathological: huge alternating offsets, a delay spike,
            // and a fake "monotonic" timestamp that jumps around. None of this
            // may influence how many frames the clock emits or when.
            let off = if i % 2 == 0 {
                9_000_000_000
            } else {
                -9_000_000_000
            };
            let now = i.saturating_mul(7) - 3;
            let _ = tracker.observe(PtpSample::new(off, 50_000_000), now);
            tracker.tick(now);
        }
        let t = clock.tick();
        out.push(t.pts);
    }
    // Whatever the tracker did, the badge state is just an estimate — never a
    // pacing input. (We read it only to prove the tracker actually ran.)
    if feed_tracker {
        let _ = tracker.state();
    }
    out
}

#[test]
fn tracker_activity_does_not_change_the_tick_stream() {
    const TICKS: i64 = 100_000;
    let baseline = run(TICKS, false);
    let with_ptp = run(TICKS, true);
    assert_eq!(
        baseline.len(),
        with_ptp.len(),
        "the PTP tracker must not change how many frames the output clock emits"
    );
    assert_eq!(
        baseline, with_ptp,
        "out_pts = f(tick) must be byte-identical with the PTP tracker churning vs. off"
    );
}

#[test]
fn absent_samples_leave_the_clock_untouched() {
    // The "PTP off" path (no samples at all) and a tracker that only ever ticks
    // (reference absent -> stays Freerun) both leave the clock identical.
    let mut clock_a = OutputClock::new(Rational::FPS_59_94).expect("valid cadence");
    let mut clock_b = OutputClock::new(Rational::FPS_59_94).expect("valid cadence");
    let mut tracker = ReferenceTracker::new(cfg());
    for i in 0..10_000i64 {
        // Tracker only ever sees the passage of time, never a sample.
        tracker.tick(i.saturating_mul(16_683_350));
        assert_eq!(clock_a.tick().pts, clock_b.tick().pts);
    }
    // No sample ever arrived: the reference never leaves Freerun.
    assert_eq!(tracker.state(), LockState::Freerun);
}
