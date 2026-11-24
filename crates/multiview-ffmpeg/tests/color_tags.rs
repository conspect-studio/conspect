//! Wire-level colour-tag verification for issue #161.
//!
//! MPEG-2 TS does not signal the quantization-range flag; ffprobe reports `tv`
//! even when the encoder context was tagged `pc`. Test H.264 in MPEG-TS and
//! verify which context tags x264 writes into sequence metadata. The test is
//! GPL-gated and does not affect the default LGPL-only build.
#![cfg(all(feature = "ffmpeg", feature = "gpl-codecs"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::Command;

use ffmpeg::codec::packet::Flags;
use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use multiview_core::time::Rational;
use multiview_ffmpeg::{Muxer, VideoEncodeTarget, VideoEncoder};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn gray_frame(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, WIDTH, HEIGHT);
    for plane in 0..frame.planes() {
        frame.data_mut(plane).fill(128);
    }
    frame.set_pts(Some(pts));
    frame
}

fn encode_segment(path: &std::path::Path, color: Option<ColorInfo>) {
    let color_options = multiview_ffmpeg::encode_options::CodecOptions::new();
    let target = VideoEncodeTarget {
        codec_name: "libx264".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, 25),
        bit_rate: 800_000,
        gop: 12,
        cuda_device: None,
        color,
    };
    let mut encoder =
        VideoEncoder::new_with_options(&target, &color_options).expect("open libx264");
    let time_base = encoder.time_base();
    let mut muxer = Muxer::create_as(path, "mpegts").expect("create transport-stream muxer");
    let stream_index = muxer
        .add_stream(encoder.as_codec_context(), time_base)
        .expect("add stream");
    muxer.write_header().expect("write header");
    for tick in 0..6 {
        let frame = gray_frame(tick);
        encoder.send_frame(&frame).expect("send frame");
        while let Some(mut packet) = encoder.receive_packet().expect("receive packet") {
            // The muxer needs a keyframe when emitting the first TS video packet.
            packet.set_flags(Flags::KEY);
            muxer
                .write_packet(stream_index, packet)
                .expect("write packet");
        }
    }
    encoder.send_eof().expect("send EOF");
    while let Some(mut packet) = encoder.receive_packet().expect("drain packet") {
        packet.set_flags(Flags::KEY);
        muxer
            .write_packet(stream_index, packet)
            .expect("write drained packet");
    }
    muxer.finish().expect("finish container");
}

fn h264_vui_flags(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "trace", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-bsf:v",
            "trace_headers",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("run ffmpeg trace_headers");
    assert!(
        output.status.success(),
        "trace_headers failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = String::from_utf8_lossy(&output.stderr);
    println!(
        "H.264 trace_headers output captured ({} bytes)",
        trace.len()
    );
    trace
        .lines()
        .filter_map(|line| {
            let (_, field) = line.rsplit_once("] ")?;
            let (description, value) = field.rsplit_once(" = ")?;
            let name = description.split_whitespace().rev().nth(1)?;
            Some((name.to_owned(), value.trim().to_owned()))
        })
        .collect()
}

fn ffprobe_stream_fields(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_range,color_space,color_primaries,color_transfer",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

#[test]
fn h264_bitstream_signals_all_four_sdr_bt709_limited_axes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tagged.ts");
    encode_segment(&path, Some(bt709_limited()));
    let fields = ffprobe_stream_fields(&path);
    println!("ffprobe stream fields: {fields:?}");
    let vui = h264_vui_flags(&path);
    assert_eq!(
        vui.get("video_signal_type_present_flag")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        vui.get("video_full_range_flag").map(String::as_str),
        Some("0")
    );
    assert_eq!(fields.get("color_range").map(String::as_str), Some("tv"));
    assert_eq!(fields.get("color_space").map(String::as_str), Some("bt709"));
    assert_eq!(
        fields.get("color_primaries").map(String::as_str),
        Some("bt709")
    );
    assert_eq!(
        fields.get("color_transfer").map(String::as_str),
        Some("bt709")
    );

    // Discriminating full-range control: ffprobe's tv default cannot make
    // both distinct encoder-context profiles produce the same report.
    let mut full_range = bt709_limited();
    full_range.range = ColorRange::Full;
    let full_path = dir.path().join("full-range.ts");
    encode_segment(&full_path, Some(full_range));
    let full_fields = ffprobe_stream_fields(&full_path);
    println!("ffprobe full-range control fields: {full_fields:?}");
    assert_eq!(
        full_fields.get("color_range").map(String::as_str),
        Some("pc"),
        "full-range profile must be distinguishable on wire"
    );
}
