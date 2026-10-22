//! DEV-C1 (ADR-M010): `EXT-X-PROGRAM-DATE-TIME` on HLS — every segment's
//! start wall time derived from the **same** outbound presentation epoch the
//! control WS publishes (`wall = epoch.wall_at(segment first PTS)`).
//!
//! Pins:
//! * the pure integer ISO 8601 formatter (exact strings — no float, no chrono);
//! * `Segment` PDT rendering (tag placement + exact text);
//! * the `LivePlaylist` epoch wiring: a fixed epoch fixture yields exact PDT
//!   tags on the published manifest; no epoch ⇒ no PDT (manifest unchanged).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_output::hls::{
    format_program_date_time, LivePlaylist, MediaPlaylist, Segment, SegmentType,
};
use multiview_output::SharedEpoch;

/// The outbound-epoch media timebase: output-PTS nanoseconds.
fn rate_ns() -> Rational {
    Rational::new(1_000_000_000, 1)
}

// ---------------------------------------------------------------------------
// The pure ISO 8601 formatter (UTC, millisecond precision, `Z`)
// ---------------------------------------------------------------------------

#[test]
fn pdt_formats_the_unix_epoch() {
    assert_eq!(format_program_date_time(0), "1970-01-01T00:00:00.000Z");
}

#[test]
fn pdt_formats_known_instants_exactly() {
    // 1_750_000_000 s = 2025-06-15T15:06:40Z (independently verified).
    assert_eq!(
        format_program_date_time(1_750_000_000_000_000_000 + 123_000_000),
        "2025-06-15T15:06:40.123Z"
    );
    // 1_781_049_600 s = 2026-06-10T00:00:00Z.
    assert_eq!(
        format_program_date_time(1_781_049_600_000_000_000),
        "2026-06-10T00:00:00.000Z"
    );
    // Leap-day handling: 1_709_251_199 s = 2024-02-29T23:59:59Z.
    assert_eq!(
        format_program_date_time(1_709_251_199_000_000_000 + 999_000_000),
        "2024-02-29T23:59:59.999Z"
    );
    // Century non-leap rules: 951_827_696 s = 2000-02-29T12:34:56Z.
    assert_eq!(
        format_program_date_time(951_827_696_000_000_000),
        "2000-02-29T12:34:56.000Z"
    );
    // End of a year: 946_684_799 s = 1999-12-31T23:59:59Z.
    assert_eq!(
        format_program_date_time(946_684_799_000_000_000),
        "1999-12-31T23:59:59.000Z"
    );
}

#[test]
fn pdt_truncates_to_milliseconds() {
    // Sub-millisecond precision is floored, never rounded up across a second.
    assert_eq!(
        format_program_date_time(1_781_049_600_123_999_999),
        "2026-06-10T00:00:00.123Z"
    );
}

#[test]
fn pdt_saturates_sanely_at_the_i64_extreme() {
    // i64::MAX ns = 2262-04-11T23:47:16.854775807Z.
    assert_eq!(
        format_program_date_time(i64::MAX),
        "2262-04-11T23:47:16.854Z"
    );
}

// ---------------------------------------------------------------------------
// Segment + MediaPlaylist rendering
// ---------------------------------------------------------------------------

#[test]
fn segment_renders_its_program_date_time_tag() {
    let mut playlist = MediaPlaylist::new(SegmentType::MpegTs);
    playlist.set_target_duration(2);
    playlist.push_segment(
        Segment::new("seg0.ts", 2.0).with_program_date_time_ns(1_781_049_600_000_000_000),
    );
    let rendered = playlist.render();
    let expected = "#EXTM3U\n\
                    #EXT-X-VERSION:7\n\
                    #EXT-X-TARGETDURATION:2\n\
                    #EXT-X-MEDIA-SEQUENCE:0\n\
                    #EXT-X-PROGRAM-DATE-TIME:2026-06-10T00:00:00.000Z\n\
                    #EXTINF:2.000,\n\
                    seg0.ts\n";
    assert_eq!(rendered, expected);
}

#[test]
fn segment_without_pdt_renders_unchanged() {
    let mut playlist = MediaPlaylist::new(SegmentType::MpegTs);
    playlist.set_target_duration(2);
    playlist.push_segment(Segment::new("seg0.ts", 2.0));
    assert!(
        !playlist.render().contains("PROGRAM-DATE-TIME"),
        "no epoch, no PDT tag"
    );
}

// ---------------------------------------------------------------------------
// LivePlaylist: PDT stamped from the shared epoch (the fixture test)
// ---------------------------------------------------------------------------

#[test]
fn live_playlist_stamps_every_segment_from_the_shared_epoch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");
    let epoch = SharedEpoch::new();
    // Epoch fixture: output pts 0 ns == 2026-06-10T00:00:00.000Z exactly.
    epoch.set(WallClockRef::new(1_781_049_600_000_000_000, 0, rate_ns()));

    let mut live = LivePlaylist::new(playlist_path.clone(), 6);
    live.set_epoch_source(epoch);
    for (i, start_pts_ns) in [0i64, 2_000_000_000, 4_000_000_000].iter().enumerate() {
        let name = format!("seg{i}.ts");
        let path = dir.path().join(&name);
        std::fs::write(&path, b"x").expect("segment file");
        live.push_closed_segment(name, path, 2.0, *start_pts_ns)
            .expect("rolling publish");
    }

    let manifest = std::fs::read_to_string(&playlist_path).expect("published manifest");
    assert!(
        manifest.contains(
            "#EXT-X-PROGRAM-DATE-TIME:2026-06-10T00:00:00.000Z\n#EXTINF:2.000,\nseg0.ts\n"
        ),
        "segment 0 PDT must be the exact epoch instant, got:\n{manifest}"
    );
    assert!(
        manifest.contains(
            "#EXT-X-PROGRAM-DATE-TIME:2026-06-10T00:00:02.000Z\n#EXTINF:2.000,\nseg1.ts\n"
        ),
        "segment 1 PDT = wall_at(2s), got:\n{manifest}"
    );
    assert!(
        manifest.contains(
            "#EXT-X-PROGRAM-DATE-TIME:2026-06-10T00:00:04.000Z\n#EXTINF:2.000,\nseg2.ts\n"
        ),
        "segment 2 PDT = wall_at(4s), got:\n{manifest}"
    );
}

#[test]
fn an_epoch_step_marks_the_next_closed_segment_discontinuous() {
    // wall-clock-sync §3: an epoch re-anchor STEP follows the same Class-2
    // discontinuity rules as a `D` change — the playlist must carry
    // `EXT-X-DISCONTINUITY` on the first segment closed under the new map.
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");
    let epoch = SharedEpoch::new();
    epoch.set(WallClockRef::new(1_781_049_600_000_000_000, 0, rate_ns()));

    let mut live = LivePlaylist::new(playlist_path.clone(), 6);
    live.set_epoch_source(epoch.clone());

    let p0 = dir.path().join("seg0.ts");
    std::fs::write(&p0, b"x").expect("segment file");
    live.push_closed_segment("seg0.ts", p0, 2.0, 0)
        .expect("publish");

    // The sampler observed a gross discontinuity (>250 ms): a Class-2-like
    // re-anchor, published as a STEPPED epoch (new generation).
    epoch.set_stepped(WallClockRef::new(1_781_049_601_000_000_000, 0, rate_ns()));
    let p1 = dir.path().join("seg1.ts");
    std::fs::write(&p1, b"x").expect("segment file");
    live.push_closed_segment("seg1.ts", p1, 2.0, 2_000_000_000)
        .expect("publish");

    // A subsequent hold/slew republish (same generation) marks nothing more.
    epoch.set(WallClockRef::new(1_781_049_601_000_001_000, 0, rate_ns()));
    let p2 = dir.path().join("seg2.ts");
    std::fs::write(&p2, b"x").expect("segment file");
    live.push_closed_segment("seg2.ts", p2, 2.0, 4_000_000_000)
        .expect("publish");

    let manifest = std::fs::read_to_string(&playlist_path).expect("manifest");
    assert!(
        manifest.contains("#EXT-X-DISCONTINUITY\n"),
        "an epoch step must emit EXT-X-DISCONTINUITY, got:\n{manifest}"
    );
    assert_eq!(
        manifest.matches("#EXT-X-DISCONTINUITY\n").count(),
        1,
        "exactly the stepped boundary is discontinuous, got:\n{manifest}"
    );
    let disc_at = manifest
        .find("#EXT-X-DISCONTINUITY\n")
        .expect("tag present");
    let seg1_at = manifest.find("seg1.ts").expect("seg1 listed");
    let seg0_at = manifest.find("seg0.ts").expect("seg0 listed");
    assert!(
        seg0_at < disc_at && disc_at < seg1_at,
        "the tag precedes the first segment closed under the stepped map, got:\n{manifest}"
    );
}

#[test]
fn held_and_slewed_epoch_updates_mark_no_discontinuity() {
    // Hold/slew keep one continuous map (bounded movement, same generation):
    // no EXT-X-DISCONTINUITY may appear.
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");
    let epoch = SharedEpoch::new();
    epoch.set(WallClockRef::new(1_781_049_600_000_000_000, 0, rate_ns()));

    let mut live = LivePlaylist::new(playlist_path.clone(), 6);
    live.set_epoch_source(epoch.clone());

    let p0 = dir.path().join("seg0.ts");
    std::fs::write(&p0, b"x").expect("segment file");
    live.push_closed_segment("seg0.ts", p0, 2.0, 0)
        .expect("publish");

    // A slewed re-anchor (≤ slew bound) republishes through `set`.
    epoch.set(WallClockRef::new(1_781_049_600_000_005_000, 0, rate_ns()));
    let p1 = dir.path().join("seg1.ts");
    std::fs::write(&p1, b"x").expect("segment file");
    live.push_closed_segment("seg1.ts", p1, 2.0, 2_000_000_000)
        .expect("publish");

    let manifest = std::fs::read_to_string(&playlist_path).expect("manifest");
    assert!(
        !manifest.contains("DISCONTINUITY"),
        "hold/slew must not mark a discontinuity, got:\n{manifest}"
    );
}

#[test]
fn live_playlist_without_an_epoch_emits_no_pdt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");
    // No epoch source set at all: behaviour is byte-identical to before.
    let mut live = LivePlaylist::new(playlist_path.clone(), 6);
    let path = dir.path().join("seg0.ts");
    std::fs::write(&path, b"x").expect("segment file");
    live.push_closed_segment("seg0.ts", path, 2.0, 0)
        .expect("rolling publish");
    let manifest = std::fs::read_to_string(&playlist_path).expect("published manifest");
    assert!(!manifest.contains("PROGRAM-DATE-TIME"));
}

#[test]
fn live_playlist_with_an_empty_epoch_cell_emits_no_pdt_until_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");
    let epoch = SharedEpoch::new();
    let mut live = LivePlaylist::new(playlist_path.clone(), 6);
    live.set_epoch_source(epoch.clone());

    // Before the first epoch publication: no PDT (the run just started and the
    // 1 Hz sampler has not yet anchored) — never a fabricated wall time.
    let p0 = dir.path().join("seg0.ts");
    std::fs::write(&p0, b"x").expect("segment file");
    live.push_closed_segment("seg0.ts", p0, 2.0, 0)
        .expect("publish");
    assert!(!std::fs::read_to_string(&playlist_path)
        .expect("manifest")
        .contains("PROGRAM-DATE-TIME"));

    // Once the epoch lands, subsequent segments are stamped.
    epoch.set(WallClockRef::new(1_781_049_600_000_000_000, 0, rate_ns()));
    let p1 = dir.path().join("seg1.ts");
    std::fs::write(&p1, b"x").expect("segment file");
    live.push_closed_segment("seg1.ts", p1, 2.0, 2_000_000_000)
        .expect("publish");
    let manifest = std::fs::read_to_string(&playlist_path).expect("manifest");
    assert!(
        manifest.contains("#EXT-X-PROGRAM-DATE-TIME:2026-06-10T00:00:02.000Z"),
        "got:\n{manifest}"
    );
}
