//! Integration tests for the per-input wall-clock USE-path rebase
//! (`PtsNormalizer`, ADR-0038 SYNC-0).
//!
//! The USE path anchors the source's first frame to its detected wall-clock
//! (`wallclock_ref.wall_at(raw_pts)`) instead of `master_now`, making a Trusted
//! HLS source's `media_time` wall-clock-accurate. The DISCARD / None path is the
//! as-built reclock-to-house behaviour, and these tests prove it is BYTE-IDENTICAL
//! to the existing `normalize()` (the rebase changes only the anchor, never the
//! per-frame delta / monotonic-guard / 33-bit unwrap).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{MediaTime, Rational};
use multiview_core::wallclock::WallClockRef;
use multiview_input::normalize::{PtsNormalizer, WrapBits};

/// The 90 kHz MPEG-TS timebase (1/90000 s per raw tick).
fn ts_tb() -> Rational {
    Rational::new(1, 90_000)
}

/// 25 fps cadence.
fn fps_25() -> Rational {
    Rational::new(25, 1)
}

/// A PDT-derived ref: PDT instant at the first segment sample (media PTS 0 ticks,
/// 90 kHz media rate). `2024-01-01T00:00:00Z` = `1_704_067_200` s.
fn pdt_ref() -> WallClockRef {
    WallClockRef::new(1_704_067_200_000_000_000, 0, Rational::new(90_000, 1))
}

#[test]
fn use_path_anchors_media_time_to_the_pdt_wallclock() {
    let mut norm = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());
    // Master clock "now" is some arbitrary monotonic instant — under USE it must
    // NOT be the anchor; the PDT wall-clock must be.
    let master_now = 5_000_000_000_i64;
    // First frame: raw PTS 0 ticks (PDT bound to PTS 0).
    let t0 = norm
        .normalize_wallclock(Some(0), master_now, Some(&pdt_ref()))
        .expect("normalize first");
    assert_eq!(
        t0.as_nanos(),
        1_704_067_200_000_000_000,
        "USE path must anchor the first frame to the PDT wall-clock, not master_now"
    );
    // Second frame one media-second later (90_000 ticks) -> wall-clock advances
    // by exactly 1 s from the PDT anchor.
    let t1 = norm
        .normalize_wallclock(Some(90_000), master_now, Some(&pdt_ref()))
        .expect("normalize second");
    assert_eq!(t1.as_nanos(), 1_704_067_201_000_000_000);
}

#[test]
fn discard_path_is_byte_identical_to_existing_normalize() {
    // Drive identical raw-PTS sequences through:
    //   (a) the existing normalize() (house anchor),
    //   (b) normalize_wallclock(.., None) (Discard / no ref).
    // The emitted MediaTime stream must be byte-identical, frame for frame.
    let raws = [0_i64, 3600, 7200, 10_800, 14_400, 90_000, 180_000];
    let master_now = 42_000_000_000_i64;

    let mut house = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());
    let mut discard = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());

    for raw in raws {
        let a: MediaTime = house.normalize(Some(raw), master_now).expect("house");
        let b: MediaTime = discard
            .normalize_wallclock(Some(raw), master_now, None)
            .expect("discard");
        assert_eq!(a, b, "Discard path diverged from the as-built house anchor");
    }
}

#[test]
fn none_ref_with_choice_use_still_falls_to_house_anchor() {
    // choice=Use but NO ref available (origin None) -> must keep the house anchor,
    // identical to today (you cannot rebase onto a wall-clock that does not exist).
    let raws = [0_i64, 3600, 7200];
    let master_now = 7_000_000_000_i64;
    let mut house = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());
    let mut used_no_ref = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());
    for raw in raws {
        let a = house.normalize(Some(raw), master_now).expect("house");
        // None ref => house anchor, regardless of the operator's Use verb.
        let b = used_no_ref
            .normalize_wallclock(Some(raw), master_now, None)
            .expect("used-no-ref");
        assert_eq!(a, b);
    }
}

#[test]
fn use_path_preserves_monotonic_guard() {
    // The rebase changes only the anchor; the strict monotonic guard must be
    // preserved (inv #3). Anchor at PTS 0 -> PDT instant, then feed a duplicate raw
    // PTS: the emitted ns must still strictly increase, not stall or go backwards.
    let mut norm = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());
    let master_now = 1_000_000_000_i64;
    let t0 = norm
        .normalize_wallclock(Some(0), master_now, Some(&pdt_ref()))
        .expect("t0");
    assert_eq!(t0.as_nanos(), 1_704_067_200_000_000_000);
    let t1 = norm
        .normalize_wallclock(Some(0), master_now, Some(&pdt_ref()))
        .expect("t1");
    assert!(
        t1.as_nanos() > t0.as_nanos(),
        "monotonic guard must hold on the USE path"
    );
}

#[test]
fn use_path_preserves_33bit_wrap_unwrap() {
    // The 33-bit wrap unwrap must be preserved on the USE path: anchor just below
    // the wrap point, then advance by a true 18000 ticks (0.2 s) which wraps modulo
    // 2^33 — the output must advance by exactly 0.2 s off the PDT anchor, NOT jump
    // backwards ~26.5 h. (Mirrors the as-built `mpeg_ts_33bit_wrap_is_unwrapped`.)
    let mut norm = PtsNormalizer::new(WrapBits::Mpeg33, ts_tb(), fps_25());
    let master_now = 1_000_000_000_i64;
    let wrap = 1_i64 << 33;
    let near_top = wrap - 9_000; // 0.1 s below the wrap
                                 // The PDT ref is bound to media_anchor = near_top (the first sample's PTS).
    let wc = WallClockRef::new(
        1_704_067_200_000_000_000,
        near_top,
        Rational::new(90_000, 1),
    );
    let t0 = norm
        .normalize_wallclock(Some(near_top), master_now, Some(&wc))
        .expect("t0 near top");
    // The first sample maps exactly to the PDT instant.
    assert_eq!(t0.as_nanos(), 1_704_067_200_000_000_000);
    let after_wrap = (near_top + 18_000) & (wrap - 1);
    assert_eq!(
        after_wrap, 9_000,
        "the raw value should have wrapped to 9000"
    );
    let t1 = norm
        .normalize_wallclock(Some(after_wrap), master_now, Some(&wc))
        .expect("t1 wrapped");
    // Exactly 18000 ticks (0.2 s) of genuine advance off the PDT anchor.
    assert_eq!(t1.as_nanos() - t0.as_nanos(), 200_000_000);
}
