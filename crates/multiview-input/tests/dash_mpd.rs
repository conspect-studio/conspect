//! Golden-manifest tests for the MPEG-DASH MPD parser and the segment-selection
//! / ABR-ladder model. These run in the DEFAULT (pure-Rust) build — the parser
//! is I/O-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use core::time::Duration;

use multiview_input::dash::{parse_iso8601_duration, DashError, Mpd, PresentationType};

/// A representative on-demand MPD with a video ABR ladder and one audio set,
/// using `$RepresentationID$`/`$Number$` segment templates.
const VOD_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
     minBufferTime="PT2S" mediaPresentationDuration="PT0H10M30.5S">
  <!-- a comment that must be skipped -->
  <Period id="p0" start="PT0S">
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate initialization="v/$RepresentationID$/init.mp4"
                       media="v/$RepresentationID$/seg-$Number$.m4s"
                       timescale="90000" duration="180000" startNumber="1"/>
      <Representation id="v360" bandwidth="800000" width="640" height="360" codecs="avc1.4d401e"/>
      <Representation id="v720" bandwidth="2500000" width="1280" height="720" codecs="avc1.640028"/>
      <Representation id="v1080" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640032"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate initialization="a/$RepresentationID$/init.mp4"
                       media="a/$RepresentationID$/seg-$Number$.m4s"
                       timescale="48000" duration="96000" startNumber="1"/>
      <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

#[test]
fn mpd_parses_vod_structure() {
    let mpd = Mpd::parse(VOD_MPD).expect("valid MPD");
    assert_eq!(mpd.presentation_type, PresentationType::Static);
    assert!(!mpd.is_live());
    assert_eq!(mpd.min_buffer_time, Some(Duration::from_secs(2)));
    assert_eq!(
        mpd.media_presentation_duration,
        Some(Duration::from_secs_f64(630.5))
    );
    assert_eq!(mpd.periods.len(), 1);

    let period = &mpd.periods[0];
    assert_eq!(period.id.as_deref(), Some("p0"));
    assert_eq!(period.start, Some(Duration::ZERO));
    assert_eq!(period.adaptation_sets.len(), 2);

    let video = period.video().expect("video set");
    assert!(video.is_video());
    assert_eq!(video.representations.len(), 3);

    let audio = period.audio().expect("audio set");
    assert!(audio.is_audio());
    assert_eq!(audio.representations.len(), 1);
}

#[test]
fn mpd_abr_ladder_is_sorted_and_selectable() {
    let mpd = Mpd::parse(VOD_MPD).unwrap();
    let video = mpd.periods[0].video().unwrap();

    // Ladder rungs sorted ascending by bandwidth.
    let ladder = video.ladder();
    let bws: Vec<u64> = ladder.iter().map(|r| r.bandwidth).collect();
    assert_eq!(bws, vec![800_000, 2_500_000, 5_000_000]);

    // At 3 Mbps we pick the 2.5 Mbps (720p) rung.
    let chosen = video.select_for_bandwidth(3_000_000).unwrap();
    assert_eq!(chosen.id, "v720");
    assert_eq!(chosen.height, Some(720));

    // Below the lowest rung we fall back to the lowest (never stall).
    let low = video.select_for_bandwidth(100_000).unwrap();
    assert_eq!(low.id, "v360");

    // Above the top we pick the top rung.
    let high = video.select_for_bandwidth(10_000_000).unwrap();
    assert_eq!(high.id, "v1080");
}

#[test]
fn mpd_segment_urls_substitute_templates() {
    let mpd = Mpd::parse(VOD_MPD).unwrap();
    let video = mpd.periods[0].video().unwrap();
    let repr = video.select_for_bandwidth(2_500_000).unwrap();
    let template = video.effective_template(repr).expect("template");

    assert_eq!(
        template.initialization_url(&repr.id).unwrap(),
        "v/v720/init.mp4"
    );
    // Segment index 0 -> startNumber 1.
    assert_eq!(template.media_url(&repr.id, 0).unwrap(), "v/v720/seg-1.m4s");
    // Segment index 4 -> number 5.
    assert_eq!(template.media_url(&repr.id, 4).unwrap(), "v/v720/seg-5.m4s");

    // Segment duration: 180000 / 90000 = 2 s.
    assert_eq!(template.segment_duration(), Some(Duration::from_secs(2)));
}

#[test]
fn mpd_dynamic_is_live() {
    let live = VOD_MPD.replace("type=\"static\"", "type=\"dynamic\"");
    let mpd = Mpd::parse(&live).unwrap();
    assert!(mpd.is_live());
    assert_eq!(mpd.presentation_type, PresentationType::Dynamic);
}

#[test]
fn mpd_time_addressing_is_rejected_without_timeline() {
    let xml = r#"<MPD type="static"><Period><AdaptationSet contentType="video">
      <Representation id="v" bandwidth="1000">
        <SegmentTemplate media="v/$Time$.m4s" timescale="90000"/>
      </Representation></AdaptationSet></Period></MPD>"#;
    let mpd = Mpd::parse(xml).unwrap();
    let repr = &mpd.periods[0].video().unwrap().representations[0];
    let template = repr.segment_template.as_ref().unwrap();
    assert!(matches!(
        template.media_url("v", 0),
        Err(DashError::Selection(_))
    ));
}

#[test]
fn mpd_rejects_non_mpd_root() {
    assert!(matches!(
        Mpd::parse("<SMIL></SMIL>"),
        Err(DashError::NotMpd)
    ));
}

#[test]
fn mpd_rejects_malformed_xml() {
    assert!(matches!(
        Mpd::parse("<MPD><Period"),
        Err(DashError::MalformedXml(_))
    ));
}

#[test]
fn iso8601_duration_parsing() {
    assert_eq!(
        parse_iso8601_duration("PT1H2M3S").unwrap(),
        Duration::from_secs(3723)
    );
    assert_eq!(
        parse_iso8601_duration("PT2.5S").unwrap(),
        Duration::from_secs_f64(2.5)
    );
    assert_eq!(
        parse_iso8601_duration("P1DT0H").unwrap(),
        Duration::from_secs(86_400)
    );
    assert!(parse_iso8601_duration("2S").is_err()); // missing leading P
    assert!(parse_iso8601_duration("PTXS").is_err()); // non-numeric
}

#[test]
fn mpd_rejects_unbalanced_element_budget() {
    // A pathological manifest of many tiny elements must hit the bounded budget,
    // not run unbounded.
    let mut huge = String::from("<MPD>");
    for _ in 0..200_000 {
        huge.push_str("<x/>");
    }
    huge.push_str("</MPD>");
    assert!(matches!(
        Mpd::parse(&huge),
        Err(DashError::MalformedXml("element budget exceeded"))
    ));
}
