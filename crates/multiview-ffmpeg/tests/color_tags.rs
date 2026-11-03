//! Output color-tagging integration test (issue #161, the `ffmpeg` feature).
//!
//! The engine's output encode must tag colour (AGENTS.md: *"tag the output and
//! verify with `ffprobe`"*) — range/primaries/transfer/matrix must land on the
//! produced stream, not `unknown`. Hardware encoders (NVENC/VAAPI/QSV) cannot
//! be exercised in this environment, so this test proves the shared
//! encoder-setup seam (`VideoEncoder::new_with_options`, used by every backend)
//! with the always-present LGPL software codec `mpeg2video`: the same path a
//! `*_nvenc`/`*_vaapi` codec name would open through.
//!
//! Verified by re-reading a produced MPEG-TS segment with `ffprobe`
//! (`-show_frames`), exactly the acceptance criterion's gate.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use multiview_core::time::Rational;
use multiview_ffmpeg::{Muxer, VideoEncodeTarget, VideoEncoder};
use tempfile::TempDir;

const W: u32 = 128;
const H: u32 = 96;
const RATE: i64 = 25;
const FRAMES: i64 = 6;

/// The `sdr-bt709-limited` canvas profile named in the issue: BT.709
/// primaries/transfer/matrix, limited range.
fn sdr_bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

/// A flat mid-gray YUV420P frame stamped with `pts` (no input file needed).
fn gray_yuv420p(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, W, H);
    for p in 0..frame.planes() {
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Open an `mpeg2video` encoder tagged with `color` through
/// [`VideoEncoder::new_with_options`] — the one shared encoder-setup path
/// every backend (software, `*_nvenc`, `*_vaapi`/`*_qsv`) opens through, so
/// proving the tag lands here proves it for every backend's codec-context
/// setup — and mux `FRAMES` synthetic frames into an MPEG-TS segment at
/// `path`.
fn encode_tagged_segment(color: ColorInfo, path: &Path) {
    let target = VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, RATE),
        bit_rate: 800_000,
        gop: 12,
        color,
        cuda_device: None,
    };
    let mut encoder = VideoEncoder::new(&target).expect("open mpeg2video encoder");
    let time_base = encoder.time_base();
    let mut muxer = Muxer::create_as(path, "mpegts").expect("create ts muxer");
    let stream_index = muxer
        .add_stream(encoder.as_codec_context(), time_base)
        .expect("add stream");
    muxer.write_header().expect("write header");

    for tick in 0..FRAMES {
        let frame = gray_yuv420p(tick);
        encoder.send_frame(&frame).expect("send frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv packet") {
            muxer.write_packet(stream_index, pkt).expect("write packet");
        }
    }
    encoder.send_eof().expect("flush encoder");
    while let Some(pkt) = encoder.receive_packet().expect("drain packet") {
        muxer
            .write_packet(stream_index, pkt)
            .expect("write drained");
    }
    muxer.finish().expect("write trailer");
}

/// `ffprobe -show_frames` the first decoded video frame's four color fields.
fn ffprobe_first_frame_color(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=color_range,color_space,color_primaries,color_transfer",
            "-read_intervals",
            "%+#1",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("ffprobe stdout is utf8")
}

#[test]
fn encoded_segment_carries_sdr_bt709_limited_color_tags() {
    // Issue #161's reproduction: an untagged encode reads back as
    // `color_range=unknown|color_space=unknown|color_primaries=unknown|color_transfer=unknown`.
    // Tagging through `VideoEncodeTarget::color` must make every one of the
    // four axes land as BT.709 limited, not unknown.
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("tagged.ts");
    encode_tagged_segment(sdr_bt709_limited(), &out);

    let frame = ffprobe_first_frame_color(&out);
    assert!(
        frame.contains("color_range=tv"),
        "range must be limited (tv) — ffprobe said: {frame}"
    );
    assert!(
        frame.contains("color_space=bt709"),
        "matrix must be bt709 — ffprobe said: {frame}"
    );
    assert!(
        frame.contains("color_primaries=bt709"),
        "primaries must be bt709 — ffprobe said: {frame}"
    );
    assert!(
        frame.contains("color_transfer=bt709"),
        "transfer must be bt709 — ffprobe said: {frame}"
    );
    assert!(
        !frame.contains("unknown"),
        "no color axis may read back unknown — ffprobe said: {frame}"
    );
}

#[test]
fn encoded_segment_with_unspecified_color_stays_unknown() {
    // The control case: an untagged `ColorInfo::default()` (all axes
    // Unspecified) must NOT be guessed into a tag — libav reports it as
    // unknown, exactly reproducing the issue's pre-fix symptom and proving
    // the tagging step never invents a value for an axis the caller did not
    // resolve.
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("untagged.ts");
    encode_tagged_segment(ColorInfo::default(), &out);

    let frame = ffprobe_first_frame_color(&out);
    // `mpeg2video` reports `color_range=tv` even when the context's
    // `color_range` field is `AVCOL_RANGE_UNSPECIFIED` (its MPEG-2 bitstream
    // has no "full range" flag to signal the alternative, so ffprobe's
    // decode-side default for an unspecified range is MPEG/tv) — that codec
    // quirk is not this seam's concern. The three axes libav's CICP code
    // points *can* leave unresolved must stay unknown.
    assert!(
        frame.contains("color_space=unknown"),
        "unspecified matrix must stay unknown — ffprobe said: {frame}"
    );
    assert!(
        frame.contains("color_primaries=unknown"),
        "unspecified primaries must stay unknown — ffprobe said: {frame}"
    );
    assert!(
        frame.contains("color_transfer=unknown"),
        "unspecified transfer must stay unknown — ffprobe said: {frame}"
    );
}
