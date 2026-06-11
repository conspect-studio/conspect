//! Embedded vs generated timecode display model (SMPTE ST 12 / RP 188).
//! HH:MM:SS:FF formatting, drop-frame handling for 29.97, frame<->count
//! round-trips, and the embedded-source field model (ATC/VITC/LTC).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_overlay::timecode::{TcRate, TcSource, Timecode, TimecodeModel};

#[test]
fn formats_non_drop_frame_with_colon_separator() {
    let tc = Timecode::new(1, 2, 3, 4, false);
    assert_eq!(tc.to_string(), "01:02:03:04");
}

#[test]
fn formats_drop_frame_with_semicolon_separator() {
    // SMPTE convention: drop-frame uses ';' before the frames field.
    let tc = Timecode::new(1, 2, 3, 4, true);
    assert_eq!(tc.to_string(), "01:02:03;04");
}

#[test]
fn frame_count_round_trips_non_drop_25fps() {
    let rate = TcRate::Fps25;
    // 1 hour at 25 fps = 90000 frames.
    let tc = Timecode::from_frame_count(90_000, rate);
    assert_eq!(tc.to_string(), "01:00:00:00");
    assert_eq!(tc.to_frame_count(rate), 90_000);
}

#[test]
fn frame_count_round_trips_30fps_non_drop() {
    let rate = TcRate::Fps30;
    let tc = Timecode::from_frame_count(108_000, rate); // 1h at 30
    assert_eq!(tc.to_string(), "01:00:00:00");
    assert_eq!(tc.to_frame_count(rate), 108_000);
}

#[test]
fn drop_frame_skips_first_two_frames_each_minute() {
    let rate = TcRate::Fps2997Drop;
    // Frame 0 -> 00:00:00;00.
    assert_eq!(
        Timecode::from_frame_count(0, rate).to_string(),
        "00:00:00;00"
    );
    // At the first minute boundary (1800 real frames) drop-frame numbering jumps
    // from ...;29 to 00:01:00;02 (frames ;00 and ;01 are skipped in labelling).
    let tc = Timecode::from_frame_count(1800, rate);
    assert_eq!(tc.to_string(), "00:01:00;02");
}

#[test]
fn drop_frame_does_not_skip_on_tenth_minute() {
    let rate = TcRate::Fps2997Drop;
    // 10 minutes of 29.97 drop-frame = 10*60*30 - 9*2 = 17982 frames.
    let tc = Timecode::from_frame_count(17_982, rate);
    assert_eq!(tc.to_string(), "00:10:00;00");
}

#[test]
fn drop_frame_one_hour_is_correct() {
    let rate = TcRate::Fps2997Drop;
    // One hour of 29.97 DF = 6 * (10 min block of 17982) = 107892 frames.
    let tc = Timecode::from_frame_count(107_892, rate);
    assert_eq!(tc.to_string(), "01:00:00;00");
    assert_eq!(tc.to_frame_count(rate), 107_892);
}

#[test]
fn tc_rate_exposes_exact_rational_cadence() {
    assert_eq!(TcRate::Fps25.cadence(), Rational::FPS_25);
    assert_eq!(TcRate::Fps30.cadence(), Rational::FPS_30);
    assert_eq!(TcRate::Fps2997Drop.cadence(), Rational::FPS_29_97);
    // Never a float fps.
    assert_eq!(TcRate::Fps2997Drop.nominal_frames(), 30);
    assert!(TcRate::Fps2997Drop.is_drop_frame());
    assert!(!TcRate::Fps30.is_drop_frame());
}

#[test]
fn tc_source_labels_are_text() {
    // a11y: the source of a displayed timecode is named in text.
    assert_eq!(TcSource::Ltc.label(), "LTC");
    assert_eq!(TcSource::Vitc.label(), "VITC");
    assert_eq!(TcSource::AtcRp188.label(), "ATC");
    assert_eq!(TcSource::Generated.label(), "GEN");
}

#[test]
fn embedded_source_is_distinguished_from_generated() {
    assert!(TcSource::Ltc.is_embedded());
    assert!(TcSource::Vitc.is_embedded());
    assert!(TcSource::AtcRp188.is_embedded());
    assert!(!TcSource::Generated.is_embedded());
}

#[test]
fn model_prefers_embedded_when_present_else_generated() {
    // Embedded ATC present: display it, tagged ATC.
    let embedded = Timecode::new(10, 0, 0, 0, false);
    let generated = Timecode::new(0, 0, 5, 0, false);
    let model = TimecodeModel::new(generated).with_embedded(TcSource::AtcRp188, embedded);
    let (src, tc) = model.displayed();
    assert_eq!(src, TcSource::AtcRp188);
    assert_eq!(tc.to_string(), "10:00:00:00");

    // No embedded TC: fall back to generated.
    let model2 = TimecodeModel::new(generated);
    let (src2, tc2) = model2.displayed();
    assert_eq!(src2, TcSource::Generated);
    assert_eq!(tc2.to_string(), "00:00:05:00");
}

#[test]
fn model_round_trips_through_json() {
    let model = TimecodeModel::new(Timecode::new(0, 1, 2, 3, false))
        .with_embedded(TcSource::Vitc, Timecode::new(9, 8, 7, 6, false));
    let json = serde_json::to_string(&model).unwrap();
    let back: TimecodeModel = serde_json::from_str(&json).unwrap();
    assert_eq!(model, back);
}
