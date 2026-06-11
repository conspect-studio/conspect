//! GP-6 Piece B — the `Muxer` `AVOption` surface (ADR-0030 §4 "Re-stamp rule").
//!
//! A guarded passthrough sets muxer options BEFORE `write_header`:
//! `avoid_negative_ts=make_zero` (a ONE-SHOT leading shift — not a mid-stream
//! monotonicity fix) and `max_interleave_delta=<n>` (an interleave-flush knob —
//! NOT the abort guard; the abort guard is Piece A's `last_dts + 1` clamp). The
//! ffmpeg-gated tests prove libav ACCEPTS the option (a bad key would error),
//! writing a header + a couple of packets to both an mp4 (DTS-strict) and an
//! mpegts (non-strict) target.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{Muxer, VideoEncodeTarget, VideoEncoder};
use tempfile::TempDir;

const W: u32 = 64;
const H: u32 = 48;
const RATE: i64 = 25;

fn target(codec: &str) -> VideoEncodeTarget {
    VideoEncodeTarget {
        codec_name: codec.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, RATE),
        bit_rate: 500_000,
        gop: 12,
        cuda_device: None,
    }
}

fn gray_yuv420p(pts: i64) -> ffmpeg::util::frame::Video {
    let mut frame = ffmpeg::util::frame::Video::new(Pixel::YUV420P, W, H);
    for p in 0..frame.planes() {
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode a few gray frames, push them through a `Muxer` built with the given
/// options + forced format, write the trailer, and assert the container exists
/// and is non-empty. The whole sequence (header_with-options + writes + trailer)
/// must succeed — a rejected option key would surface as an `Err` here.
fn mux_with_options(dir: &Path, file: &str, format_name: &str, opts: &[(&str, &str)]) {
    let path = dir.join(file);
    let mut enc = VideoEncoder::new(&target("mpeg2video")).expect("open mpeg2video");
    let tb = enc.time_base();

    let mut muxer =
        Muxer::create_with_options(&path, format_name, opts).expect("create muxer with options");
    let vidx = muxer
        .add_stream(enc.as_codec_context(), tb)
        .expect("add video stream");
    muxer
        .write_header()
        .expect("write_header must accept the options");

    // Encode 5 frames and mux every emitted packet.
    let mut wrote = 0_u32;
    for tick in 0..5_i64 {
        enc.send_frame(&gray_yuv420p(tick)).expect("send frame");
        while let Some(pkt) = enc.receive_packet().expect("recv packet") {
            muxer.write_packet(vidx, pkt).expect("write packet");
            wrote += 1;
        }
    }
    enc.send_eof().expect("flush encoder");
    while let Some(pkt) = enc.receive_packet().expect("drain") {
        muxer.write_packet(vidx, pkt).expect("write drained packet");
        wrote += 1;
    }
    muxer.finish().expect("write trailer");

    assert!(wrote > 0, "muxed at least one packet");
    let meta = std::fs::metadata(&path).expect("output file exists");
    assert!(meta.len() > 0, "output container is non-empty");
}

#[test]
fn mp4_accepts_avoid_negative_ts_make_zero() {
    let dir = TempDir::new().expect("tempdir");
    // mp4 is DTS-strict; avoid_negative_ts=make_zero is the one-shot leading
    // shift a guarded passthrough sets so leading negative timestamps land at 0.
    mux_with_options(
        dir.path(),
        "out.mp4",
        "mp4",
        &[
            ("avoid_negative_ts", "make_zero"),
            ("max_interleave_delta", "0"),
        ],
    );
}

#[test]
fn mpegts_accepts_avoid_negative_ts_make_zero() {
    let dir = TempDir::new().expect("tempdir");
    // mpegts is non-strict on DTS; the same option surface must still apply.
    mux_with_options(
        dir.path(),
        "out.ts",
        "mpegts",
        &[("avoid_negative_ts", "make_zero")],
    );
}

#[test]
fn no_options_behaves_like_create_as() {
    let dir = TempDir::new().expect("tempdir");
    // An empty option slice is the additive identity: same result as create_as.
    mux_with_options(dir.path(), "plain.mp4", "mp4", &[]);
}

#[test]
fn rejected_option_key_surfaces_an_error() {
    // libav must reject a nonsense option at write_header — never silently accept
    // and never panic. (avformat_write_header returns the leftover dict; the
    // Muxer treats a non-empty leftover as a typed error.)
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("bad.mp4");
    let enc = VideoEncoder::new(&target("mpeg2video")).expect("open enc");
    let tb = enc.time_base();
    let mut muxer =
        Muxer::create_with_options(&path, "mp4", &[("this_is_not_a_real_muxer_option", "1")])
            .expect("create");
    muxer
        .add_stream(enc.as_codec_context(), tb)
        .expect("add stream");
    let res = muxer.write_header();
    assert!(
        res.is_err(),
        "an unknown muxer option must surface a typed error, got Ok"
    );
}
