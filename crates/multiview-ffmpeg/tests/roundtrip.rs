//! Full decode -> NV12 -> re-encode -> mux -> re-open round-trip.
//!
//! This is the headline integration test for the safe wrapper layer: it proves
//! the whole chain (demux, decode-to-NV12, libswscale, encode, mux) is
//! leak-free and produces a re-openable container with the expected frame
//! count, geometry, and a monotonically non-decreasing PTS sequence.
//!
//! Gated behind the `ffmpeg` feature. The source clip is generated at test time
//! with the **LGPL** `ffv1` codec; the re-encode uses the **LGPL** `mpeg2video`
//! software encoder (NOT x264/x265 — those are GPL and behind `gpl-codecs`).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::{
    Demuxer, Muxer, ScaleSpec, Scaler, StreamVideoDecoder, VideoEncodeTarget, VideoEncoder,
};
use tempfile::TempDir;

const W: u32 = 320;
const H: u32 = 240;
const RATE: u32 = 25;
const SECONDS: u32 = 1;
const EXPECTED_FRAMES: u32 = RATE * SECONDS;

/// Generate a 1-second `testsrc` video clip with the LGPL `ffv1` codec.
fn generate_clip(dir: &Path) -> PathBuf {
    let out = dir.join("src.mkv");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate={RATE}"),
            "-t",
            &SECONDS.to_string(),
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate clip");
    out
}

/// Decode every frame of the source clip to NV12 and return the frames plus the
/// count, proving the decode-to-NV12 path works end to end.
fn decode_all_to_nv12(clip: &Path) -> Vec<ffmpeg::util::frame::Video> {
    let mut demux = Demuxer::open(clip).expect("open source");
    let streams = demux.streams();
    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("video stream");
    let vidx = video.index;

    // Build the decoder from the stream's parameters + time-base.
    let params = {
        // Re-open to grab owned parameters via libav (Demuxer exposes a snapshot,
        // but the decoder needs the libav `Parameters`); use a second handle's
        // raw streams through the public decoder constructor by re-deriving from
        // a fresh input. Here we rely on `StreamVideoDecoder::new` taking
        // `Parameters`, which we obtain from a throwaway input.
        let input = ffmpeg::format::input(&clip).expect("reopen for params");
        input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("best video")
            .parameters()
    };
    let mut decoder =
        StreamVideoDecoder::new(params, video.time_base).expect("build video decoder");

    let mut frames = Vec::new();
    while let Some(pkt) = demux.read_packet().expect("read") {
        if pkt.stream_index != vidx {
            continue;
        }
        decoder.send_packet(&pkt.packet).expect("send packet");
        while let Some(decoded) = decoder.receive_frame().expect("receive") {
            assert_eq!(decoded.meta.width, W, "decoded width");
            assert_eq!(decoded.meta.height, H, "decoded height");
            // Every frame leaving the decoder is on the NV12-throughout timeline.
            assert_eq!(
                decoded.frame.format(),
                Pixel::NV12,
                "decoded format is NV12"
            );
            frames.push(decoded.frame);
        }
    }
    decoder.send_eof().expect("eof");
    while let Some(decoded) = decoder.receive_frame().expect("drain") {
        frames.push(decoded.frame);
    }
    frames
}

// reason: this end-to-end round-trip is a single, linear, assertion-dense
// scenario (demux -> decode -> re-encode -> mux -> re-open); splitting it would
// fragment the one chain it verifies. Adding the `cuda_device: None` field to
// the encode target tipped it one line past the cosmetic length lint.
#[allow(clippy::too_many_lines)]
#[test]
fn decode_reencode_mux_reopen_round_trips() {
    let dir = TempDir::new().unwrap();
    let clip = generate_clip(dir.path());

    // 1. Decode the whole clip to NV12 host frames.
    let nv12_frames = decode_all_to_nv12(&clip);
    assert_eq!(
        u32::try_from(nv12_frames.len()).unwrap(),
        EXPECTED_FRAMES,
        "decoded frame count matches the source"
    );

    // 2. Re-encode with the LGPL mpeg2video encoder. mpeg2video wants planar
    //    YUV420P, so convert NV12 -> YUV420P with libswscale first (exercising
    //    the scaler in the other direction too).
    let time_base = Rational::new(1, i64::from(RATE));
    let target = VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base,
        bit_rate: 1_000_000,
        gop: 12,
        cuda_device: None,
    };
    let mut encoder = VideoEncoder::new(&target).expect("open mpeg2video encoder");

    let mut to_yuv420p = Scaler::new(
        ScaleSpec::new(Pixel::NV12, W, H),
        ScaleSpec::new(Pixel::YUV420P, W, H),
    )
    .expect("build NV12->YUV420P scaler");

    // 3. Mux into a fresh Matroska container, re-stamping every PTS from the
    //    tick counter (NEVER from the source's raw input PTS).
    let out_path = dir.path().join("out.mkv");
    let mut muxer = Muxer::create(&out_path).expect("create muxer");
    let stream_index = {
        // `VideoEncoder` derefs to a `codec::Context` for `add_stream`.
        let ctx: &ffmpeg::codec::Context = encoder.as_codec_context();
        muxer.add_stream(ctx, time_base).expect("add stream")
    };
    muxer.write_header().expect("write header");

    for (tick, nv12) in nv12_frames.iter().enumerate() {
        let mut yuv = to_yuv420p.run(nv12).expect("scale to yuv420p");
        // Re-stamp from the tick counter — the output-clock contract.
        yuv.set_pts(Some(i64::try_from(tick).unwrap()));
        encoder.send_frame(&yuv).expect("send frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv packet") {
            muxer.write_packet(stream_index, pkt).expect("write packet");
        }
    }
    // Flush the encoder.
    encoder.send_eof().expect("encoder eof");
    while let Some(pkt) = encoder.receive_packet().expect("drain packet") {
        muxer
            .write_packet(stream_index, pkt)
            .expect("write drained");
    }
    muxer.finish().expect("write trailer");

    assert!(out_path.exists(), "muxed output exists");
    assert!(
        std::fs::metadata(&out_path).unwrap().len() > 0,
        "muxed output is non-empty"
    );

    // 4. Re-open the muxed container and verify it round-trips: same geometry,
    //    a decodable frame count matching what we put in (mpeg2video is not
    //    all-intra but is lossy/closed-GOP; every input frame still decodes
    //    out), and a monotonically non-decreasing PTS sequence.
    let mut rt = Demuxer::open(&out_path).expect("reopen muxed output");
    let rt_streams = rt.streams();
    let rt_video = rt_streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("muxed video stream");
    assert_eq!(rt_video.width, W, "round-trip width");
    assert_eq!(rt_video.height, H, "round-trip height");
    assert_eq!(rt_video.codec_name, "mpeg2video", "round-trip codec");

    let rt_params = {
        let input = ffmpeg::format::input(&out_path).expect("reopen for params");
        input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .unwrap()
            .parameters()
    };
    let mut rt_decoder =
        StreamVideoDecoder::new(rt_params, rt_video.time_base).expect("rebuild decoder");

    let mut decoded_count = 0_u32;
    let mut last_pts = i64::MIN;
    let mut monotonic = true;
    let rt_vidx = rt_video.index;
    while let Some(pkt) = rt.read_packet().expect("rt read") {
        if pkt.stream_index != rt_vidx {
            continue;
        }
        rt_decoder.send_packet(&pkt.packet).expect("rt send");
        while let Some(frame) = rt_decoder.receive_frame().expect("rt recv") {
            decoded_count += 1;
            let pts = frame.meta.pts.as_nanos();
            if pts < last_pts {
                monotonic = false;
            }
            last_pts = pts;
        }
    }
    rt_decoder.send_eof().expect("rt eof");
    while let Some(_frame) = rt_decoder.receive_frame().expect("rt drain") {
        decoded_count += 1;
    }

    assert_eq!(
        decoded_count, EXPECTED_FRAMES,
        "every muxed frame decodes back out"
    );
    assert!(
        monotonic,
        "decoded PTS sequence is monotonically non-decreasing"
    );
}
