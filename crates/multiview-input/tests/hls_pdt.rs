//! Integration tests for HLS `EXT-X-PROGRAM-DATE-TIME` extraction → a
//! `WallClockRef`, and the per-source wall-clock trust classification (ADR-0038,
//! SYNC-0 + the HLS extraction of SYNC-1).
//!
//! These pin RFC 8216 §4.3.2.6: a media playlist's PDT instant binds the FIRST
//! sample of the segment it precedes, so the affine map maps that sample's media
//! PTS to the PDT wall-clock. Trust is classified by mirroring the engine's
//! lock-state tolerance pattern (in-tolerance + monotonic ⇒ Trusted; a jump /
//! non-monotonic PDT ⇒ Suspected; absent ⇒ None).
//!
//! Fixtures are synthetic; NO private hostnames / feed URLs appear (this is the
//! public repo).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_core::wallclock::{WallClockOrigin, WallClockTier};
use multiview_input::hls::{MediaPlaylist, PdtTrustConfig};

/// 90 kHz media timebase (HLS fMP4 / TS PTS are 90 kHz).
fn rate_90k() -> Rational {
    Rational::new(90_000, 1)
}

/// A live HLS media playlist whose first segment is anchored to a PDT instant.
/// `2024-01-01T00:00:00.000Z` = `1_704_067_200` s past the Unix epoch.
const MEDIA_WITH_PDT: &str = "#EXTM3U\n\
    #EXT-X-VERSION:3\n\
    #EXT-X-TARGETDURATION:6\n\
    #EXT-X-MEDIA-SEQUENCE:0\n\
    #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:00:00.000Z\n\
    #EXTINF:6.000,\n\
    seg0.ts\n\
    #EXTINF:6.000,\n\
    seg1.ts\n\
    #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:00:12.000Z\n\
    #EXTINF:6.000,\n\
    seg2.ts\n";

#[test]
fn media_playlist_parses_first_pdt_instant_to_unix_ns() {
    let media = MediaPlaylist::parse(MEDIA_WITH_PDT).expect("parse media playlist");
    // The first PDT precedes seg0 -> the playlist's earliest anchor.
    let first = media.first_program_date_time().expect("a PDT anchor");
    // 1_704_067_200 s * 1e9 ns/s = 1_704_067_200_000_000_000 ns.
    assert_eq!(first.wall_ns, 1_704_067_200_000_000_000);
    // It precedes the first media segment (segment index 0).
    assert_eq!(first.segment_index, 0);
    // A second PDT anchor precedes seg2 (index 2).
    let anchors: Vec<_> = media.program_date_times().collect();
    assert_eq!(anchors.len(), 2);
    assert_eq!(anchors[1].segment_index, 2);
    assert_eq!(anchors[1].wall_ns, 1_704_067_212_000_000_000);
}

#[test]
fn pdt_binds_first_sample_media_pts_into_an_affine_wallclock_ref() {
    let media = MediaPlaylist::parse(MEDIA_WITH_PDT).expect("parse");
    let first = media.first_program_date_time().expect("a PDT");
    // The first sample of seg0 has media PTS 0 (90 kHz ticks) on this source.
    // Build the ref binding PDT(seg0) <-> that first-sample PTS.
    let wc = first.wallclock_ref(0, rate_90k());
    // wall(0) == the PDT instant exactly.
    assert_eq!(wc.wall_at(0), 1_704_067_200_000_000_000);
    // 90_000 ticks (1 media-second) later -> 1 s of wall-clock later.
    assert_eq!(wc.wall_at(90_000), 1_704_067_201_000_000_000);
}

#[test]
fn pdt_present_and_monotonic_classifies_trusted() {
    let media = MediaPlaylist::parse(MEDIA_WITH_PDT).expect("parse");
    let trust = media.classify_trust(&PdtTrustConfig::default());
    assert_eq!(trust.tier, WallClockTier::Trusted);
    assert_eq!(trust.origin, WallClockOrigin::ProgramDateTime);
}

#[test]
fn pdt_that_jumps_backwards_classifies_suspected() {
    // The second PDT is EARLIER than the first (a non-monotonic jump) -> the
    // wall-clock assertion is implausible, so trust degrades to Suspected.
    let text = "#EXTM3U\n\
        #EXT-X-TARGETDURATION:6\n\
        #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:00:30.000Z\n\
        #EXTINF:6.000,\n\
        seg0.ts\n\
        #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:00:00.000Z\n\
        #EXTINF:6.000,\n\
        seg1.ts\n";
    let media = MediaPlaylist::parse(text).expect("parse");
    let trust = media.classify_trust(&PdtTrustConfig::default());
    assert_eq!(trust.tier, WallClockTier::Suspected);
    assert_eq!(trust.origin, WallClockOrigin::ProgramDateTime);
}

#[test]
fn pdt_that_drifts_far_from_segment_durations_classifies_suspected() {
    // Two PDTs separated by 6 s of segments but asserting a 600 s wall gap: the
    // wall-clock assertion is out of tolerance vs the media timeline -> Suspected.
    let text = "#EXTM3U\n\
        #EXT-X-TARGETDURATION:6\n\
        #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:00:00.000Z\n\
        #EXTINF:6.000,\n\
        seg0.ts\n\
        #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:10:00.000Z\n\
        #EXTINF:6.000,\n\
        seg1.ts\n";
    let media = MediaPlaylist::parse(text).expect("parse");
    let trust = media.classify_trust(&PdtTrustConfig::default());
    assert_eq!(trust.tier, WallClockTier::Suspected);
}

#[test]
fn absent_pdt_classifies_none() {
    let text = "#EXTM3U\n\
        #EXT-X-TARGETDURATION:6\n\
        #EXTINF:6.000,\n\
        seg0.ts\n\
        #EXTINF:6.000,\n\
        seg1.ts\n";
    let media = MediaPlaylist::parse(text).expect("parse");
    assert!(media.first_program_date_time().is_none());
    let trust = media.classify_trust(&PdtTrustConfig::default());
    assert_eq!(trust.tier, WallClockTier::None);
    assert_eq!(trust.origin, WallClockOrigin::None);
}

#[test]
fn fractional_and_offset_pdt_timestamps_parse_to_exact_ns() {
    // RFC 3339 with milliseconds and a +HH:MM offset (not Z).
    let text = "#EXTM3U\n\
        #EXT-X-PROGRAM-DATE-TIME:2024-01-01T10:00:00.250+10:00\n\
        #EXTINF:2.000,\n\
        seg0.ts\n";
    let media = MediaPlaylist::parse(text).expect("parse");
    let first = media.first_program_date_time().expect("a PDT");
    // 2024-01-01T10:00:00.250+10:00 == 2024-01-01T00:00:00.250Z
    // == 1_704_067_200 s + 250 ms.
    assert_eq!(first.wall_ns, 1_704_067_200_000_000_000 + 250_000_000);
}

#[test]
fn non_playlist_text_is_rejected() {
    assert!(MediaPlaylist::parse("not a playlist\n").is_err());
}
