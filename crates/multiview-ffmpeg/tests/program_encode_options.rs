//! Pure program-encode option-set tests (no `ffmpeg` feature needed).
//!
//! The program (multiview canvas) encoder must not inherit libav's per-version
//! encoder defaults for anything that changes the emitted stream. `FFmpeg` 9.0
//! silently moved `h264_nvenc`'s default profile from Main to High, so the
//! profile is pinned explicitly here per H.264 encoder family. These pairs are
//! pure data over [`CodecOptions`], unit-testable without libav.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::{program_h264_options, CodecOptions, PROGRAM_H264_PROFILE};

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
fn program_profile_is_high() {
    assert_eq!(PROGRAM_H264_PROFILE, "high");
}

#[test]
fn nvenc_pins_high_profile() {
    assert_eq!(
        pairs(&program_h264_options("h264_nvenc")),
        expect(&[("profile", "high")])
    );
}

#[test]
fn vaapi_pins_high_profile() {
    assert_eq!(
        pairs(&program_h264_options("h264_vaapi")),
        expect(&[("profile", "high")])
    );
}

#[test]
fn qsv_pins_high_profile() {
    assert_eq!(
        pairs(&program_h264_options("h264_qsv")),
        expect(&[("profile", "high")])
    );
}

#[test]
fn videotoolbox_pins_high_profile() {
    assert_eq!(
        pairs(&program_h264_options("h264_videotoolbox")),
        expect(&[("profile", "high")])
    );
}

#[test]
fn libx264_pins_high_profile() {
    assert_eq!(
        pairs(&program_h264_options("libx264")),
        expect(&[("profile", "high")])
    );
}

#[test]
fn non_h264_encoders_get_no_profile_pair() {
    // A `profile` value is codec-specific ("high" is meaningless or invalid for
    // HEVC, MPEG-2, FFV1, ...): emit nothing rather than fail an open or apply
    // an H.264 name to another codec.
    for name in [
        "hevc_nvenc",
        "mpeg2video",
        "ffv1",
        "mjpeg",
        "libx265",
        "rawvideo",
    ] {
        assert!(
            program_h264_options(name).as_pairs().is_empty(),
            "{name} must get no program profile pair"
        );
    }
}
