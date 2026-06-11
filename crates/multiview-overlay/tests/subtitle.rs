//! Integration tests for the pure SRT/WebVTT subtitle ingest (Stage 3, ADR-R007):
//! parse a cue document into a time-indexed [`CueTrack`] and resolve the cue
//! active at a given media time. REAL assertions: an SRT cue spanning 1s..3s is
//! the active cue at t=2s and carries the expected text; markup is stripped; a
//! VTT track with a header parses the same; the ASS capability gate reports the
//! graceful fallback.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_core::time::MediaTime;
use multiview_overlay::libass::{AssCapability, SubtitleFallback};
use multiview_overlay::subtitle::{CueTrack, SubtitleFormat};

/// A media time at `secs` whole seconds.
fn at_secs(secs: i64) -> MediaTime {
    MediaTime::from_nanos(secs.saturating_mul(1_000_000_000))
}

#[test]
fn srt_cue_at_two_seconds_produces_the_expected_text_run() {
    // A cue visible from 00:00:01,000 to 00:00:03,000 must be the active cue at
    // t = 2 s and carry exactly its text lines (the renderer turns these into a
    // text run via the stage-1 engine).
    let srt = "1\n\
               00:00:01,000 --> 00:00:03,000\n\
               Hello, world\n\
               \n\
               2\n\
               00:00:04,000 --> 00:00:06,000\n\
               Second cue\n";
    let track = CueTrack::parse_srt(srt).expect("valid SRT parses");
    assert_eq!(track.format(), SubtitleFormat::SubRip);
    assert_eq!(track.len(), 2, "two cues parsed");

    let active = track
        .active_cue(at_secs(2))
        .expect("a cue is active at t=2s");
    assert_eq!(active.lines, vec!["Hello, world".to_owned()]);
    assert_eq!(active.text(), "Hello, world");

    // The window is half-open: visible at its start, not at its end.
    assert!(track.active_cue(at_secs(1)).is_some(), "active at start");
    assert!(
        track
            .active_cue(MediaTime::from_nanos(3_000_000_000))
            .is_none(),
        "the 1..3 cue is gone exactly at its end (3s); the next starts at 4s"
    );

    // The second cue is active at t=5s, not the first.
    let later = track.active_cue(at_secs(5)).expect("active at t=5s");
    assert_eq!(later.text(), "Second cue");

    // No cue before the first.
    assert!(track.active_cue(at_secs(0)).is_none(), "nothing before 1s");
}

#[test]
fn srt_multiline_and_markup_is_stripped() {
    let srt = "1\n\
               00:00:00,500 --> 00:00:02,500\n\
               <i>Line one</i>\n\
               <font color=\"#ff0000\">Line two</font>\n";
    let track = CueTrack::parse_srt(srt).expect("valid SRT");
    let cue = track.cues().first().expect("one cue");
    assert_eq!(
        cue.lines,
        vec!["Line one".to_owned(), "Line two".to_owned()],
        "two lines, markup tags removed"
    );
    // Fractional milliseconds parse: 0.5s start.
    assert_eq!(cue.start, MediaTime::from_nanos(500_000_000));
    assert_eq!(cue.end, MediaTime::from_nanos(2_500_000_000));
}

#[test]
fn webvtt_header_and_dot_fraction_parse() {
    let vtt = "WEBVTT\n\
               \n\
               00:00:02.000 --> 00:00:04.000 position:50%\n\
               <c.loud>VTT caption</c>\n";
    let track = CueTrack::parse_vtt(vtt).expect("valid VTT");
    assert_eq!(track.format(), SubtitleFormat::WebVtt);
    assert_eq!(track.len(), 1, "header is not a cue");
    let active = track.active_cue(at_secs(3)).expect("active at 3s");
    assert_eq!(active.text(), "VTT caption", "VTT cue tags stripped");
    // Trailing cue settings (position:50%) do not break end-time parsing.
    assert_eq!(active.end, MediaTime::from_nanos(4_000_000_000));
}

#[test]
fn out_of_order_window_is_rejected() {
    let srt = "1\n00:00:03,000 --> 00:00:01,000\nbad\n";
    assert!(
        CueTrack::parse_srt(srt).is_err(),
        "end before start must error, not silently produce a never-visible cue"
    );
}

#[test]
fn ass_capability_falls_back_to_plain_text_without_libass() {
    // The default build is pure-Rust (no `libass` feature), so ASS rendering is
    // unavailable and the engine degrades to the plain-text SRT/VTT path — never
    // a hard failure (graceful degradation, ADR-R007).
    let cap = AssCapability::detect();
    assert_eq!(cap, AssCapability::Unavailable);
    assert!(!cap.is_available());
    assert_eq!(cap.fallback(), SubtitleFallback::PlainText);
}
