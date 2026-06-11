//! Integration tests for the per-input PTS normalizer (invariant #3).
//!
//! These pin the unified timing model: 33-bit MPEG-TS / 32-bit RTP wrap unwrap,
//! genpts fallback, monotonic guard, rebase onto the internal ns timeline, and
//! discontinuity re-anchor. The normalizer MUST never emit a backwards ns
//! timestamp and MUST unwrap correctly across the wrap boundary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_input::normalize::{PtsNormalizer, WrapBits};

/// The 90 kHz MPEG-TS timebase as a `Rational` (1/90000 s per tick).
fn ts_tb() -> Rational {
    Rational::new(1, 90_000)
}

#[test]
fn first_frame_anchors_to_master_now() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    // anchor at master_now = 5s, first raw pts = 9000 ticks (0.1s @ 90kHz)
    let out = n.normalize(Some(9_000), 5_000_000_000).unwrap();
    // First frame ALWAYS rebases to the anchor (master_now), not the raw value.
    assert_eq!(out.as_nanos(), 5_000_000_000);
}

#[test]
fn subsequent_frames_advance_by_pts_delta_from_anchor() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let _ = n.normalize(Some(0), 5_000_000_000).unwrap();
    // +9000 ticks @ 90kHz = +0.1s; relative to anchor 5s -> 5.1s.
    let out = n.normalize(Some(9_000), 5_000_000_000).unwrap();
    assert_eq!(out.as_nanos(), 5_100_000_000);
}

#[test]
fn monotonic_guard_rejects_backwards_pts() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let _ = n.normalize(Some(0), 0).unwrap();
    let t1 = n.normalize(Some(9_000), 0).unwrap();
    // A small backwards step (90 ticks = 1ms back) is NOT a discontinuity; the
    // monotonic guard must bump it to last + 1 ns, never go backwards.
    let t2 = n.normalize(Some(8_910), 0).unwrap();
    assert!(
        t2.as_nanos() > t1.as_nanos(),
        "guard must enforce strictly increasing ns: {} !> {}",
        t2.as_nanos(),
        t1.as_nanos()
    );
    assert_eq!(t2.as_nanos(), t1.as_nanos() + 1);
}

#[test]
fn equal_pts_is_bumped_to_strictly_increasing() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let t0 = n.normalize(Some(9_000), 0).unwrap();
    let t1 = n.normalize(Some(9_000), 0).unwrap();
    assert_eq!(t1.as_nanos(), t0.as_nanos() + 1);
}

#[test]
fn mpeg_ts_33bit_wrap_is_unwrapped() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let wrap = 1_i64 << 33;
    // Anchor 9000 ticks (0.1s) below the wrap point.
    let near_top = wrap - 9_000;
    let t0 = n.normalize(Some(near_top), 0).unwrap();
    // Advance a TRUE 18000 ticks (0.2s); the raw value wraps modulo 2^33 to 9000.
    let after_wrap = (near_top + 18_000) & (wrap - 1);
    assert_eq!(
        after_wrap, 9_000,
        "the raw value should have wrapped to 9000"
    );
    let t1 = n.normalize(Some(after_wrap), 0).unwrap();
    // Output must advance by exactly the genuine elapsed 18000 ticks = 0.2s,
    // NOT jump backwards ~26.5h.
    assert_eq!(t1.as_nanos() - t0.as_nanos(), 200_000_000);
}

#[test]
fn rtp_32bit_wrap_is_unwrapped() {
    let mut n = PtsNormalizer::new(WrapBits::Rtp32, ts_tb(), Rational::FPS_25);
    let wrap = 1_i64 << 32;
    let near_top = wrap - 9_000;
    let t0 = n.normalize(Some(near_top), 0).unwrap();
    let after_wrap = (near_top + 18_000) & (wrap - 1);
    assert_eq!(
        after_wrap, 9_000,
        "the raw value should have wrapped to 9000"
    );
    let t1 = n.normalize(Some(after_wrap), 0).unwrap();
    assert_eq!(t1.as_nanos() - t0.as_nanos(), 200_000_000);
}

#[test]
fn genpts_fallback_synthesizes_missing_pts_from_cadence() {
    // No PTS available (AV_NOPTS): synthesize from the declared cadence.
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let t0 = n.normalize(None, 1_000_000_000).unwrap();
    assert_eq!(t0.as_nanos(), 1_000_000_000);
    // Next missing PTS advances by one frame period at 25fps = 40ms.
    let t1 = n.normalize(None, 1_000_000_000).unwrap();
    assert_eq!(t1.as_nanos() - t0.as_nanos(), 40_000_000);
    let t2 = n.normalize(None, 1_000_000_000).unwrap();
    assert_eq!(t2.as_nanos() - t1.as_nanos(), 40_000_000);
}

#[test]
fn genpts_continues_after_real_pts() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let t0 = n.normalize(Some(0), 0).unwrap();
    // Real pts +9000 ticks = +0.1s.
    let t1 = n.normalize(Some(9_000), 0).unwrap();
    assert_eq!(t1.as_nanos() - t0.as_nanos(), 100_000_000);
    // Now a missing pts: continues from last by one frame period.
    let t2 = n.normalize(None, 0).unwrap();
    assert_eq!(t2.as_nanos() - t1.as_nanos(), 40_000_000);
}

#[test]
fn large_forward_jump_re_anchors_smoothly() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let _ = n.normalize(Some(0), 0).unwrap();
    let t1 = n.normalize(Some(9_000), 0).unwrap(); // +0.1s
                                                   // A 1-hour forward jump in raw PTS is a discontinuity, not real elapsed
                                                   // time. Re-anchor: the output continues smoothly (small step), not +1h.
    let one_hour_ticks = 90_000_i64 * 3600;
    let t2 = n.normalize(Some(9_000 + one_hour_ticks), 0).unwrap();
    let step = t2.as_nanos() - t1.as_nanos();
    assert!(
        step < 1_000_000_000,
        "discontinuity must re-anchor, not propagate a 1h jump: step={step}ns"
    );
    assert!(step > 0, "must still advance monotonically");
}

#[test]
fn explicit_discontinuity_re_anchors() {
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_25);
    let _ = n.normalize(Some(0), 0).unwrap();
    let t1 = n.normalize(Some(9_000), 0).unwrap();
    // Signal an EXT-X-DISCONTINUITY: the next raw PTS resets to a small/odd
    // value (RFC 8216 allows any value, even descending). The normalizer must
    // re-anchor and continue smoothly forward.
    n.mark_discontinuity();
    let t2 = n.normalize(Some(50), 0).unwrap();
    assert!(
        t2.as_nanos() > t1.as_nanos(),
        "must advance after discontinuity re-anchor"
    );
    let step = t2.as_nanos() - t1.as_nanos();
    assert!(step < 1_000_000_000, "re-anchor step should be small");
}

#[test]
fn ntsc_cadence_uses_exact_rational_no_drift() {
    // genpts at 30000/1001 fps: 1000 synthesized frames must equal exactly
    // 1000 * 1001/30000 s in ns, with no float drift.
    let mut n = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), Rational::FPS_29_97);
    let t0 = n.normalize(None, 0).unwrap();
    let mut last = t0;
    for _ in 0..1000 {
        last = n.normalize(None, 0).unwrap();
    }
    // Expected total = 1000 frame-periods. period = 1001/30000 s.
    // 1000 * 1001/30000 s = 1001/30 s = 33366666666.67 ns -> nearest.
    let expected = multiview_core::time::rescale(
        1000,
        Rational::new(1001, 30_000),
        Rational::new(1, 1_000_000_000),
    );
    let got = last.as_nanos() - t0.as_nanos();
    // Allow at most 1000 ns total accumulated rounding (1 ns per frame guard).
    assert!(
        (got - expected).abs() <= 1000,
        "NTSC genpts drift too large: got {got} expected {expected}"
    );
}
