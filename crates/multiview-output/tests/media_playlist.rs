//! Golden-string integration tests for HLS media-playlist generation.
//!
//! These pin the *exact* serialized text of generated playlists. Because HLS
//! playlists are plain UTF-8 manifests consumed by third-party players, the
//! byte-for-byte output is part of our contract — so every assertion here is an
//! exact string match, not a structural approximation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::hls::{MediaPlaylist, Part, Segment, SegmentType, ServerControl};

/// A vanilla VOD-style finished playlist renders the canonical tag order and
/// terminates with `#EXT-X-ENDLIST`.
#[test]
fn finished_playlist_golden() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.set_target_duration(4);
    pl.set_media_sequence(0);
    pl.push_segment(Segment::new("seg0.m4s", 4.0));
    pl.push_segment(Segment::new("seg1.m4s", 4.0));
    pl.push_segment(Segment::new("seg2.m4s", 3.5));
    pl.set_finished(true);

    let expected = "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:4.000,
seg0.m4s
#EXTINF:4.000,
seg1.m4s
#EXTINF:3.500,
seg2.m4s
#EXT-X-ENDLIST
";
    assert_eq!(pl.render(), expected);
}

/// A live playlist (not finished) omits `#EXT-X-ENDLIST` and never emits the
/// terminator until `set_finished(true)` is called.
#[test]
fn live_playlist_has_no_endlist() {
    let mut pl = MediaPlaylist::new(SegmentType::MpegTs);
    pl.set_target_duration(2);
    pl.set_media_sequence(10);
    pl.push_segment(Segment::new("seg10.ts", 2.0));

    let out = pl.render();
    assert!(
        !out.contains("#EXT-X-ENDLIST"),
        "live playlist must not end: {out}"
    );
    // MPEG-TS playlists carry no EXT-X-MAP init segment.
    assert!(
        !out.contains("#EXT-X-MAP"),
        "ts playlist has no init map: {out}"
    );
    assert!(out.starts_with("#EXTM3U\n"));
    assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:10\n"));
}

/// A discontinuity flag on a segment emits `#EXT-X-DISCONTINUITY` *before* the
/// EXTINF for that segment, and the discontinuity sequence is surfaced.
#[test]
fn discontinuity_golden() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.set_target_duration(2);
    pl.set_media_sequence(5);
    pl.set_discontinuity_sequence(1);
    pl.push_segment(Segment::new("a.m4s", 2.0));
    let mut disc = Segment::new("b.m4s", 2.0);
    disc.discontinuity = true;
    pl.push_segment(disc);
    pl.set_finished(true);

    let expected = "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:5
#EXT-X-DISCONTINUITY-SEQUENCE:1
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:2.000,
a.m4s
#EXT-X-DISCONTINUITY
#EXTINF:2.000,
b.m4s
#EXT-X-ENDLIST
";
    assert_eq!(pl.render(), expected);
}

/// The sliding window keeps at most `window` segments, advancing
/// `EXT-X-MEDIA-SEQUENCE` by the number of evicted segments so the manifest
/// stays internally consistent (msn always points at the first listed segment).
#[test]
fn sliding_window_advances_media_sequence() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.set_target_duration(2);
    pl.set_window(3);
    for i in 0..6u32 {
        pl.push_segment(Segment::new(format!("seg{i}.m4s"), 2.0));
    }
    // 6 pushed, window 3 => first 3 evicted => media-sequence == 3.
    assert_eq!(pl.media_sequence(), 3);
    let out = pl.render();
    assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:3\n"), "{out}");
    assert!(out.contains("seg3.m4s"));
    assert!(out.contains("seg5.m4s"));
    assert!(!out.contains("seg0.m4s"));
    assert!(!out.contains("seg2.m4s\n"));
}

/// Discontinuities that scroll out of the window bump the discontinuity
/// sequence so players can keep their timeline accounting correct.
#[test]
fn evicted_discontinuity_bumps_discontinuity_sequence() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.set_target_duration(2);
    pl.set_window(2);
    let mut a = Segment::new("a.m4s", 2.0);
    a.discontinuity = true; // discontinuity on the very first segment
    pl.push_segment(a);
    pl.push_segment(Segment::new("b.m4s", 2.0));
    pl.push_segment(Segment::new("c.m4s", 2.0)); // evicts a (the discontinuity)
    assert_eq!(pl.media_sequence(), 1);
    assert_eq!(pl.discontinuity_sequence(), 1);
}

/// `EXT-X-TARGETDURATION` is an integer; the HLS spec requires it be the
/// rounded maximum segment duration. The helper computes it from the window.
#[test]
fn target_duration_rounds_max_segment() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.push_segment(Segment::new("a.m4s", 1.4));
    pl.push_segment(Segment::new("b.m4s", 2.6)); // rounds to 3
    pl.recompute_target_duration();
    assert_eq!(pl.target_duration(), 3);
}

/// LL-HLS: parts are emitted before their parent segment's EXTINF, an
/// independent part carries `INDEPENDENT=YES`, and the trailing preload hint +
/// server-control + part-inf tags are rendered.
#[test]
fn ll_hls_part_segment_relationship_golden() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.set_target_duration(2);
    pl.set_media_sequence(0);
    pl.set_part_target(0.5);
    pl.set_server_control(ServerControl {
        can_block_reload: true,
        part_hold_back: Some(1.5),
        hold_back: None,
        can_skip_until: None,
    });

    let mut seg = Segment::new("seg0.m4s", 2.0);
    seg.parts.push(Part::new("seg0.0.m4s", 0.5).independent());
    seg.parts.push(Part::new("seg0.1.m4s", 0.5));
    seg.parts.push(Part::new("seg0.2.m4s", 0.5));
    seg.parts.push(Part::new("seg0.3.m4s", 0.5));
    pl.push_segment(seg);

    pl.set_preload_hint(Some("seg1.0.m4s".to_owned()));

    let expected = "\
#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.500
#EXT-X-PART-INF:PART-TARGET=0.500
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-MAP:URI=\"init.mp4\"
#EXT-X-PART:DURATION=0.500,URI=\"seg0.0.m4s\",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI=\"seg0.1.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"seg0.2.m4s\"
#EXT-X-PART:DURATION=0.500,URI=\"seg0.3.m4s\"
#EXTINF:2.000,
seg0.m4s
#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"seg1.0.m4s\"
";
    assert_eq!(pl.render(), expected);
}

/// Rendition reports point peers at the latest msn/part of a sibling rendition.
#[test]
fn rendition_report_golden() {
    let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
    pl.set_target_duration(2);
    pl.set_media_sequence(0);
    pl.set_part_target(0.5);
    pl.push_segment(Segment::new("seg0.m4s", 2.0));
    pl.add_rendition_report("../audio/pl.m3u8", 6, Some(0));
    pl.add_rendition_report("../alt/pl.m3u8", 5, None);

    let out = pl.render();
    assert!(
        out.contains("#EXT-X-RENDITION-REPORT:URI=\"../audio/pl.m3u8\",LAST-MSN=6,LAST-PART=0\n"),
        "{out}"
    );
    assert!(
        out.contains("#EXT-X-RENDITION-REPORT:URI=\"../alt/pl.m3u8\",LAST-MSN=5\n"),
        "{out}"
    );
}
