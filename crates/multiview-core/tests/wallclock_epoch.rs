//! DEV-C1 (ADR-M010): the **inverse** affine map `WallClockRef::media_at` —
//! wall-clock ns → media PTS — plus the tick↔wall round-trip contract the
//! outbound presentation epoch rests on.
//!
//! All arithmetic must be exact integer (`i128` intermediates via `rescale`),
//! never float: the epoch is consumed by RTCP SR stamping and HLS
//! `EXT-X-PROGRAM-DATE-TIME`, where a float drift would skew every consumer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use proptest::prelude::*;

/// The outbound-epoch media timebase: output-PTS nanoseconds (1 GHz ticks).
fn rate_ns() -> Rational {
    Rational::new(1_000_000_000, 1)
}

/// 90 kHz media timebase (RTP / MPEG-TS).
fn rate_90k() -> Rational {
    Rational::new(90_000, 1)
}

#[test]
fn media_at_recovers_the_anchor_exactly() {
    let wc = WallClockRef::new(1_781_049_600_000_000_000, 0, rate_ns());
    assert_eq!(wc.media_at(1_781_049_600_000_000_000), 0);
}

#[test]
fn media_at_is_exact_for_the_ns_rate() {
    // rate = 1e9 ticks/s means media units ARE nanoseconds: the inverse map is
    // a pure integer subtraction.
    let wc = WallClockRef::new(1_700_000_000_000_000_000, 5_000, rate_ns());
    assert_eq!(wc.media_at(1_700_000_000_000_000_123), 5_123);
    // Before the anchor: exact negative delta.
    assert_eq!(wc.media_at(1_699_999_999_999_999_877), 4_877);
}

#[test]
fn media_at_rescales_into_90khz_exactly() {
    // One second past the anchor at 90 kHz is exactly 90 000 ticks.
    let wc = WallClockRef::new(1_700_000_000_000_000_000, 500, rate_90k());
    assert_eq!(wc.media_at(1_700_000_001_000_000_000), 90_500);
    // One second BEFORE the anchor: exactly -90 000 ticks from the anchor.
    assert_eq!(wc.media_at(1_699_999_999_000_000_000), 500 - 90_000);
}

#[test]
fn ns_rate_round_trips_exactly_both_ways() {
    let wc = WallClockRef::new(1_750_000_000_000_000_000, 0, rate_ns());
    for pts in [0i64, 1, 999, 1_000_000_007, 86_400_000_000_000] {
        let wall = wc.wall_at(pts);
        assert_eq!(wc.media_at(wall), pts, "round trip must be exact at 1 GHz");
    }
}

proptest! {
    /// At the epoch's canonical 1 GHz (ns) media rate the tick↔wall round trip
    /// is EXACT for any pts within a century of the anchor.
    #[test]
    fn prop_ns_rate_round_trip_is_exact(pts in -3_155_760_000_000_000_000i64..3_155_760_000_000_000_000i64) {
        let wc = WallClockRef::new(1_750_000_000_000_000_000, 0, rate_ns());
        let wall = wc.wall_at(pts);
        prop_assert_eq!(wc.media_at(wall), pts);
    }

    /// At a coarser media rate (90 kHz) the round trip is exact to within one
    /// tick (the floor/round quantisation of wall_at), never more.
    #[test]
    fn prop_90k_round_trip_is_within_one_tick(pts in -1_000_000_000_000i64..1_000_000_000_000i64) {
        let wc = WallClockRef::new(1_750_000_000_000_000_000, 0, rate_90k());
        let wall = wc.wall_at(pts);
        let back = wc.media_at(wall);
        prop_assert!((back - pts).abs() <= 1, "pts {} -> wall {} -> {}", pts, wall, back);
    }

    /// A degenerate rate must yield the anchor media position, never panic.
    #[test]
    fn prop_degenerate_rate_never_panics(wall in any::<i64>()) {
        let wc = WallClockRef::new(0, 42, Rational::new(0, 0));
        prop_assert_eq!(wc.media_at(wall), 42);
    }
}
