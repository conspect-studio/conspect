//! End-to-end proof that the chosen libav* binding (`ffmpeg-next`, linking the
//! system libav* via `pkg-config`) compiles **and runs**: generate a tiny
//! self-contained clip with the `ffmpeg` CLI, then open it, demux, and decode
//! the first video frame through [`multiview_ffmpeg::VideoDecoder`].
//!
//! Gated behind the `ffmpeg` feature so the default pure-Rust build never
//! touches native deps. The clip is generated at test time with an **LGPL**
//! software codec (`ffv1`) — never x264/x265 — so the test stays LGPL-clean and
//! fully self-contained (no checked-in media).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg_next as ffmpeg;
use multiview_ffmpeg::{DecodedFrameInfo, VideoDecoder};
use tempfile::TempDir;

/// Width/height of the generated test pattern.
const W: u32 = 320;
const H: u32 = 240;

/// Generate a 1-second `testsrc` clip into `dir` using the `ffmpeg` CLI with
/// the LGPL `ffv1` codec, returning the path. Skips (via `panic!` that the
/// harness reports) only if the CLI is genuinely unavailable — which would mean
/// the test environment is misconfigured.
fn generate_clip(dir: &Path) -> PathBuf {
    let out = dir.join("spike.mkv");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate=25"),
            "-t",
            "1",
            // LGPL, in-tree software codec — NOT x264/x265 (GPL).
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("failed to spawn the `ffmpeg` CLI (is FFmpeg installed?)");

    assert!(
        status.success(),
        "ffmpeg CLI exited with failure while generating the test clip"
    );
    assert!(out.exists(), "ffmpeg CLI did not produce the output file");
    out
}

/// Generate a multi-second `testsrc` clip (`seconds` seconds at 25 fps) into
/// `dir` with the LGPL `ffv1` codec — all-intra, so every frame is its own
/// self-contained packet and the file holds `25 * seconds` video packets. Used
/// to prove `decode_first_frame` does not drain the whole file.
fn generate_long_clip(dir: &Path, seconds: u32) -> PathBuf {
    let out = dir.join("long.mkv");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate=25"),
            "-t",
            &seconds.to_string(),
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("failed to spawn the `ffmpeg` CLI (is FFmpeg installed?)");
    assert!(status.success(), "ffmpeg CLI failed to generate long clip");
    assert!(out.exists(), "ffmpeg CLI did not produce the long clip");
    out
}

#[test]
fn decode_first_frame_streams_and_does_not_drain_the_whole_clip() {
    // A 2-second all-intra clip holds 50 video packets. The first frame is ready
    // from the very first packet, so a STREAMING decoder must return having
    // pulled only a handful of packets. The previous implementation collected
    // *every* packet into a Vec before decoding frame 0, which for a file
    // buffers the whole file and for a live/infinite URL never returns — this
    // test fails outright against that behaviour.
    let dir = TempDir::new().expect("create tempdir");
    let clip = generate_long_clip(dir.path(), 2);

    let mut decoder = VideoDecoder::open(&clip).expect("open + build decoder for the long clip");

    // Before decoding, nothing has been consumed.
    assert_eq!(
        decoder.packets_consumed(),
        0,
        "no packets consumed before decode_first_frame runs"
    );

    let info = decoder
        .decode_first_frame()
        .expect("decode the first video frame from the long clip");
    assert_eq!(info.width, W, "decoded frame width");
    assert_eq!(info.height, H, "decoded frame height");

    let consumed = decoder.packets_consumed();
    // ffv1 is all-intra: frame 0 needs exactly one packet. Allow a small margin
    // for decoder priming, but it must be FAR below the 50 packets in the file —
    // proving the loop streams and returns early rather than draining to EOF.
    assert!(
        consumed >= 1,
        "must have pulled at least the packet carrying frame 0, got {consumed}"
    );
    assert!(
        consumed <= 5,
        "must stop after the few packets frame 0 needs, not drain all 50; got {consumed}"
    );
}

#[test]
fn decodes_first_video_frame_geometry_and_pts() {
    let dir = TempDir::new().expect("create tempdir");
    let clip = generate_clip(dir.path());

    let mut decoder = VideoDecoder::open(&clip).expect("open + build decoder for the test clip");

    let info: DecodedFrameInfo = decoder
        .decode_first_frame()
        .expect("decode the first video frame");

    // Geometry must match exactly what we asked the CLI to render.
    assert_eq!(info.width, W, "decoded frame width");
    assert_eq!(info.height, H, "decoded frame height");

    // The clip was encoded as planar 8-bit 4:2:0, so the decoded software frame
    // must be YUV420P. This is an exact assertion — not a "non-empty" smoke test.
    assert_eq!(
        info.format,
        ffmpeg::format::Pixel::YUV420P,
        "decoded pixel format"
    );

    // The very first frame of a freshly muxed stream is at the start of the
    // timeline; its PTS must be present and zero (stream time-base ticks).
    assert_eq!(
        info.pts,
        Some(0),
        "first-frame presentation timestamp (stream ticks)"
    );
}

#[test]
fn open_reports_missing_file_without_panicking() {
    // A non-existent path must surface a typed error, never panic — exercising
    // the `OpenInput` error arm and the no-unwrap-on-failure guarantee.
    let missing = Path::new("/nonexistent/multiview-ffmpeg/does-not-exist.mkv");
    // `VideoDecoder` is not `Debug` (it owns libav contexts), so match rather
    // than `expect_err`.
    let rendered = match VideoDecoder::open(missing) {
        Ok(_) => panic!("opening a missing file must fail"),
        Err(err) => err.to_string(),
    };
    assert!(
        rendered.contains("does-not-exist.mkv"),
        "error should name the offending path, got: {rendered}"
    );
}

#[test]
fn video_decoder_is_send() {
    // Compile-time assertion that the decoder can move to a decode thread.
    // (It is intentionally NOT `Sync`; libav contexts need external sync.)
    fn assert_send<T: Send>() {}
    assert_send::<VideoDecoder>();
}
