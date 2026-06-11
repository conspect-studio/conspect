//! Integration tests for the HLS / wall-clock input pacer (invariant #4).
//!
//! The pacer releases each frame at `anchor_wall + (pts - pts0)` against an
//! INJECTED clock so it is deterministically testable. `-re` is for files, not
//! live ingest — the pacer owns wall-clock pacing. Releases happen in PTS order
//! at the correct wall-clock instant; bounded catch-up smooths small drift.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_input::pacer::{Pacer, PacerConfig, Release};

fn mt(ns: i64) -> MediaTime {
    MediaTime::from_nanos(ns)
}

#[test]
fn first_frame_anchors_and_releases_immediately() {
    let mut p = Pacer::new(PacerConfig::default());
    // First frame: anchor (anchor_wall = now, pts0 = pts). Released at once.
    let r = p.submit(mt(1000), 5_000_000_000);
    assert_eq!(r, Release::Now);
}

#[test]
fn release_deadline_tracks_pts_minus_pts0_plus_anchor() {
    let mut p = Pacer::new(PacerConfig::default());
    // Anchor at wall=10s, pts0 = 1000ns.
    assert_eq!(p.submit(mt(1000), 10_000_000_000), Release::Now);
    // A frame 40ms later in PTS releases 40ms after the anchor wall time.
    let pts = mt(1000 + 40_000_000);
    let deadline = p.release_deadline(pts).unwrap();
    assert_eq!(deadline, 10_000_000_000 + 40_000_000);
}

#[test]
fn frame_held_until_wallclock_reaches_deadline() {
    let mut p = Pacer::new(PacerConfig::default());
    assert_eq!(p.submit(mt(0), 0), Release::Now);
    let pts = mt(100_000_000); // +100ms
                               // At now=50ms, not yet due.
    match p.submit(pts, 50_000_000) {
        Release::At(ns) => assert_eq!(ns, 100_000_000),
        other => panic!("expected hold, got {other:?}"),
    }
    // At now=100ms, due exactly.
    assert_eq!(p.submit(pts, 100_000_000), Release::Now);
    // At now>deadline, also Now (overdue releases immediately).
    let pts2 = mt(120_000_000);
    assert_eq!(p.submit(pts2, 200_000_000), Release::Now);
}

#[test]
fn releases_are_in_pts_order_at_correct_wallclock() {
    let mut p = Pacer::new(PacerConfig::default());
    // Anchor at wall=0, pts0=0.
    assert_eq!(p.submit(mt(0), 0), Release::Now);
    // Each subsequent frame's deadline equals its pts offset (anchor wall = 0).
    let pts_list = [
        20_000_000_i64,
        40_000_000,
        60_000_000,
        80_000_000,
        100_000_000,
    ];
    let mut last_deadline = 0_i64;
    for pts in pts_list {
        let d = p.release_deadline(mt(pts)).unwrap();
        assert_eq!(d, pts, "deadline must equal anchor_wall + pts offset");
        assert!(d > last_deadline, "deadlines must increase with pts");
        last_deadline = d;
    }
}

#[test]
fn re_anchors_on_marked_discontinuity() {
    let mut p = Pacer::new(PacerConfig::default());
    assert_eq!(p.submit(mt(0), 0), Release::Now);
    // After 1s of progress.
    let _ = p.submit(mt(1_000_000_000), 1_000_000_000);
    // Discontinuity: the source PTS resets to a small value. Re-anchor so it
    // releases immediately at the current wall time rather than waiting/seeking.
    p.mark_discontinuity();
    let r = p.submit(mt(50_000), 2_000_000_000);
    assert_eq!(
        r,
        Release::Now,
        "first frame after discontinuity re-anchors"
    );
    // Subsequent frame is paced from the NEW anchor (wall=2s, pts0=50000).
    let d = p.release_deadline(mt(50_000 + 30_000_000)).unwrap();
    assert_eq!(d, 2_000_000_000 + 30_000_000);
}

#[test]
fn large_pts_jump_re_anchors() {
    let mut p = Pacer::new(PacerConfig::default());
    assert_eq!(p.submit(mt(0), 0), Release::Now);
    // A huge forward PTS jump (> the configured discontinuity threshold) must
    // re-anchor rather than schedule a release hours in the future.
    let huge = mt(3_600_000_000_000); // +1 hour
    let r = p.submit(huge, 1_000_000_000);
    assert_eq!(r, Release::Now);
}

#[test]
fn bounded_catchup_advances_releases_when_behind() {
    // When the consumer falls behind (latency-to-edge grows), the pacer is
    // allowed to advance releases by at most catchup_rate, never instant-seek.
    let cfg = PacerConfig {
        max_catchup_rate_num: 5,
        max_catchup_rate_den: 4, // 1.25x
        ..PacerConfig::default()
    };
    let mut p = Pacer::new(cfg);
    assert_eq!(p.submit(mt(0), 0), Release::Now);
    // The effective deadline for a frame, when the pacer is asked to catch up,
    // shrinks the wall interval by the catch-up factor but never below zero.
    let pts = mt(100_000_000); // nominal +100ms
    let nominal = p.release_deadline(pts).unwrap();
    assert_eq!(nominal, 100_000_000);
    // With catch-up engaged, the deadline is pulled earlier by up to 1/1.25.
    let caught = p.release_deadline_catchup(pts);
    assert!(
        caught <= nominal,
        "catch-up may only pull deadlines earlier: {caught} <= {nominal}"
    );
    // 100ms / 1.25 = 80ms — the catch-up deadline.
    assert_eq!(caught, 80_000_000);
}
