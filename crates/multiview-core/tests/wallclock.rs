//! Integration tests for the per-source wall-clock trust + affine-map types
//! (ADR-0038, SYNC-0).
//!
//! These pin:
//! * the `WallClockRef` affine map `wall(pts) = wall_anchor + rescale(pts −
//!   media_anchor, rate)` is exact (round-trips with rationals, no float drift);
//! * `WallClockTrust` / `SyncMode` serde is internally-tagged (robust across TOML
//!   and JSON), never `untagged`;
//! * the honest rule is encoded: `SyncMode::HouseClocked` carries no
//!   `WallClockRef` (it cannot be content-synced).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_core::wallclock::{
    SyncMode, WallClockChoice, WallClockOrigin, WallClockRef, WallClockTier, WallClockTrust,
};
use proptest::prelude::*;

/// 90 kHz media timebase as a `Rational` (the RTP / MPEG-TS media rate).
fn rate_90k() -> Rational {
    Rational::new(90_000, 1)
}

#[test]
fn wallclock_ref_maps_the_anchor_sample_exactly() {
    // A PDT instant of 1_700_000_000.000_000_000 s past the Unix epoch bound to a
    // media PTS of 0 ns.
    let wall_anchor_ns = 1_700_000_000_000_000_000_i64;
    let media_anchor_ns = 0_i64;
    let wc = WallClockRef::new(
        wall_anchor_ns,
        media_anchor_ns,
        Rational::new(1, 1_000_000_000),
    );
    // The anchor sample maps to exactly the PDT instant.
    assert_eq!(wc.wall_at(media_anchor_ns), wall_anchor_ns);
}

#[test]
fn wallclock_ref_advances_one_second_per_one_second_of_media() {
    // Anchor wall at t0; media measured in 90 kHz ticks (rate = 90000/1).
    let wall_anchor_ns = 1_700_000_000_000_000_000_i64;
    let media_anchor_ticks = 0_i64;
    let wc = WallClockRef::new(wall_anchor_ns, media_anchor_ticks, rate_90k());
    // 90_000 ticks at 90 kHz = exactly 1 s later in wall-clock.
    assert_eq!(
        wc.wall_at(90_000),
        wall_anchor_ns + 1_000_000_000,
        "one media-second must advance the wall-clock by exactly 1e9 ns"
    );
    // A non-zero media anchor: deltas are measured FROM the anchor.
    let wc2 = WallClockRef::new(wall_anchor_ns, 45_000, rate_90k());
    assert_eq!(wc2.wall_at(45_000), wall_anchor_ns);
    assert_eq!(wc2.wall_at(135_000), wall_anchor_ns + 1_000_000_000);
}

#[test]
fn sync_mode_house_clocked_carries_no_ref() {
    // The honest content-sync rule: house-clocked sources cannot be content-synced.
    let house = SyncMode::HouseClocked;
    assert!(house.wallclock_ref().is_none());
    let synced = SyncMode::ContentSynced(WallClockRef::new(10, 0, Rational::new(1, 1)));
    assert!(synced.wallclock_ref().is_some());
}

#[test]
fn trust_serde_is_internally_tagged_json_and_round_trips() {
    let trust = WallClockTrust {
        tier: WallClockTier::Trusted,
        origin: WallClockOrigin::ProgramDateTime,
        choice: WallClockChoice::Use,
    };
    let json = serde_json::to_string(&trust).expect("serialize");
    // Internally/adjacently tagged — NOT a bare/untagged variant; the field names
    // are present so a foreign consumer can read it unambiguously.
    assert!(json.contains("\"tier\""), "tier field present: {json}");
    assert!(json.contains("\"origin\""), "origin field present: {json}");
    assert!(json.contains("\"choice\""), "choice field present: {json}");
    let back: WallClockTrust = serde_json::from_str(&json).expect("round-trip");
    assert_eq!(back, trust);
}

#[test]
fn tier_origin_choice_serialize_as_snake_case_tags() {
    // Each enum serializes as a stable string tag (never an integer / untagged
    // positional form), so configs + events are robust and human-readable.
    assert_eq!(
        serde_json::to_string(&WallClockTier::Suspected).unwrap(),
        "\"suspected\""
    );
    assert_eq!(
        serde_json::to_string(&WallClockOrigin::RtcpSr).unwrap(),
        "\"rtcp_sr\""
    );
    assert_eq!(
        serde_json::to_string(&WallClockChoice::Discard).unwrap(),
        "\"discard\""
    );
}

proptest! {
    /// The affine map is exact: for any anchor + plausible media delta the mapped
    /// wall-clock equals the anchor plus the exact rational rescale of the delta.
    /// No float appears anywhere, so there is no drift.
    #[test]
    fn wall_at_is_exact_affine(
        wall_anchor in -1_000_000_000_000_000i64..1_000_000_000_000_000i64,
        media_anchor in -1_000_000_000i64..1_000_000_000i64,
        delta in -1_000_000i64..1_000_000i64,
    ) {
        let wc = WallClockRef::new(wall_anchor, media_anchor, rate_90k());
        let pts = media_anchor.saturating_add(delta);
        // delta ticks at 90 kHz -> ns: a tick spans 1/90000 s, so rescale `delta`
        // from the (den/num = 1/90000 s per tick) timebase into ns, rounded.
        let tick_timebase = Rational::new(1, 90_000);
        let expected_delta_ns =
            multiview_core::time::rescale(delta, tick_timebase, Rational::new(1, 1_000_000_000));
        prop_assert_eq!(wc.wall_at(pts), wall_anchor.saturating_add(expected_delta_ns));
    }
}
