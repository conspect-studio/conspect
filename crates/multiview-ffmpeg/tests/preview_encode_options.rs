//! Pure preview-encode option-set tests (ADR-P006; no `ffmpeg` feature needed).
//!
//! ADR-P006 fixes the preview encoder settings: zerolatency-class rate control,
//! B-frames hard-off, repeat-headers (structural: the crate never sets
//! `AV_CODEC_FLAG_GLOBAL_HEADER`, so Annex-B encoders emit SPS/PPS at every
//! IDR), and a 2-second GOP. `preview_h264_options` / `preview_vp8_options`
//! produce that option set as pure data over the crate's typed
//! [`CodecOptions`], per selected encoder family — unit-testable without libav.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::{
    preview_gop_frames, preview_h264_options, preview_vp8_options, CodecOptions,
};

/// Flatten an option set into comparable `(key, value)` string pairs.
fn pairs(options: &CodecOptions) -> Vec<(String, String)> {
    options.as_pairs().to_vec()
}

fn expect(pairs_list: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs_list
        .iter()
        .map(|&(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn gop_is_two_seconds_of_frames_rounded_to_nearest() {
    // 15 fps preview cadence -> 30 frames; exact NTSC rationals stay exact
    // (invariant #3: never float fps).
    assert_eq!(preview_gop_frames(Rational::new(15, 1)), 30);
    assert_eq!(preview_gop_frames(Rational::new(25, 1)), 50);
    // 29.97 -> 59.94 frames in 2 s -> rounds to 60.
    assert_eq!(preview_gop_frames(Rational::FPS_29_97), 60);
    // 12.5 fps -> exactly 25.
    assert_eq!(preview_gop_frames(Rational::new(25, 2)), 25);
}

#[test]
fn gop_never_collapses_to_zero_on_degenerate_rates() {
    // A zero/negative/denormal rate must never produce gop 0 (which would mean
    // "codec default" downstream) — clamp to 1, never fabricate.
    assert_eq!(preview_gop_frames(Rational::new(0, 1)), 1);
    assert_eq!(preview_gop_frames(Rational::new(0, 0)), 1);
    assert_eq!(preview_gop_frames(Rational::new(-15, 1)), 1);
}

#[test]
fn nvenc_gets_ull_zerolatency_cbr_and_forced_idr() {
    let opts = preview_h264_options("h264_nvenc", Rational::new(15, 1));
    assert_eq!(
        pairs(&opts),
        expect(&[
            ("g", "30"),
            ("bf", "0"),
            ("tune", "ull"),
            ("zerolatency", "1"),
            ("delay", "0"),
            ("rc", "cbr"),
            ("forced-idr", "1"),
        ])
    );
}

#[test]
fn vaapi_gets_cbr_rate_control() {
    let opts = preview_h264_options("h264_vaapi", Rational::new(15, 1));
    assert_eq!(
        pairs(&opts),
        expect(&[
            ("g", "30"),
            ("bf", "0"),
            ("rc_mode", "CBR"),
            ("async_depth", "1"),
        ])
    );
}

#[test]
fn videotoolbox_gets_realtime() {
    let opts = preview_h264_options("h264_videotoolbox", Rational::new(15, 1));
    assert_eq!(
        pairs(&opts),
        expect(&[
            ("g", "30"),
            ("bf", "0"),
            ("realtime", "1"),
            ("prio_speed", "1"),
        ])
    );
}

#[test]
fn libx264_gets_zerolatency_tune_and_forced_idr() {
    let opts = preview_h264_options("libx264", Rational::new(15, 1));
    assert_eq!(
        pairs(&opts),
        expect(&[
            ("g", "30"),
            ("bf", "0"),
            ("tune", "zerolatency"),
            ("forced-idr", "1"),
        ])
    );
}

#[test]
fn unknown_encoder_gets_only_the_generic_set() {
    // An unrecognized encoder name must still get the codec-generic AVOptions
    // (`g`/`bf` are AVCodecContext-generic) and nothing family-specific that
    // could fail an open.
    let opts = preview_h264_options("h264_something_else", Rational::new(15, 1));
    assert_eq!(pairs(&opts), expect(&[("g", "30"), ("bf", "0")]));
}

#[test]
fn vp8_preview_options_are_realtime_no_lag_error_resilient() {
    // The libvpx software rung (ADR-P006): realtime deadline, zero lookahead
    // lag, error-resilient. VP8 has no B-frame concept, so no `bf` pair.
    let opts = preview_vp8_options(Rational::new(15, 1));
    assert_eq!(
        pairs(&opts),
        expect(&[
            ("g", "30"),
            ("deadline", "realtime"),
            ("cpu-used", "8"),
            ("lag-in-frames", "0"),
            ("error-resilient", "default"),
        ])
    );
}

#[test]
fn codec_options_validate_interior_nul() {
    // Same up-front C-string guarantee as MuxOptions: a key/value with an
    // interior NUL can never become an av_dict entry — typed error, never UB.
    assert!(CodecOptions::new().try_set("g", "30").is_ok());
    assert!(CodecOptions::new().try_set("g\0", "30").is_err());
    assert!(CodecOptions::new().try_set("g", "3\x000").is_err());
    assert!(CodecOptions::from_pairs(&[("a", "1"), ("b\0", "2")]).is_err());

    let opts = CodecOptions::from_pairs(&[("a", "1"), ("b", "2")]).unwrap();
    assert_eq!(
        opts.as_pairs(),
        &[
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned())
        ]
    );
    assert!(!opts.is_empty());
    assert!(CodecOptions::new().is_empty());
}
