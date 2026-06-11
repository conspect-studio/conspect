//! Codec-selection + real-encode integration tests (the `ffmpeg` feature).
//!
//! Proves [`multiview_ffmpeg::select_encoder`] resolves a real, openable encoder
//! for the logical [`VideoCodec`] families this build supports, and that the
//! chosen encoder actually produces a playable container that `ffprobe`
//! confirms.
//!
//! Two paths:
//! * **Default / `ffmpeg`** — `VideoCodec::Mpeg2Video` selects the LGPL software
//!   encoder `mpeg2video`; encoding + muxing to `.ts` yields an ffprobe-verified
//!   `mpeg2video` stream. H.264/H.265 select nothing (the LGPL-clean build
//!   genuinely cannot encode them).
//! * **`gpl-codecs`** — `VideoCodec::H264` selects `libx264`; encoding + muxing
//!   to `.mp4` yields an ffprobe-verified `h264` stream of the right geometry and
//!   frame count.
//!
//! The synthetic frames are generated in-process (no input clip needed); the
//! output container is verified with the `ffprobe` CLI present in the dev/CI
//! image. The only GPL codec touched (`libx264`) is behind the `gpl-codecs`
//! feature, never the default build.
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
use multiview_core::time::Rational;
use multiview_ffmpeg::{select_encoder, Muxer, VideoCodec, VideoEncodeTarget, VideoEncoder};
use tempfile::TempDir;

const W: u32 = 128;
const H: u32 = 96;
const RATE: i64 = 25;
const FRAMES: i64 = 30;

/// A flat-gray YUV420P frame stamped with `pts` (no input file needed).
fn gray_yuv420p(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, W, H);
    for p in 0..frame.planes() {
        // Plane 0 (luma) -> mid-gray; planes 1/2 (chroma) -> neutral 128.
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode `FRAMES` synthetic frames with `encoder_name` and mux them to `path`
/// using the libav `muxer_name` container. Re-stamps every PTS from the tick
/// counter (the output-clock contract). Returns the number of packets muxed.
fn encode_to_container(encoder_name: &str, muxer_name: &str, path: &Path) -> u64 {
    let target = VideoEncodeTarget {
        codec_name: encoder_name.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, RATE),
        bit_rate: 800_000,
        gop: 12,
        cuda_device: None,
    };
    let mut encoder = VideoEncoder::new(&target).expect("open selected encoder");
    let time_base = encoder.time_base();
    let mut muxer = Muxer::create_as(path, muxer_name).expect("create muxer");
    let stream_index = muxer
        .add_stream(encoder.as_codec_context(), time_base)
        .expect("add stream");
    muxer.write_header().expect("write header");

    let mut packets = 0_u64;
    for tick in 0..FRAMES {
        let frame = gray_yuv420p(tick);
        encoder.send_frame(&frame).expect("send frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv packet") {
            packets += 1;
            muxer.write_packet(stream_index, pkt).expect("write packet");
        }
    }
    encoder.send_eof().expect("flush encoder");
    while let Some(pkt) = encoder.receive_packet().expect("drain packet") {
        packets += 1;
        muxer
            .write_packet(stream_index, pkt)
            .expect("write drained");
    }
    muxer.finish().expect("write trailer");
    packets
}

/// Run `ffprobe` and return the FIRST non-empty line of its stdout, asserting it
/// succeeded.
///
/// An MPEG-TS container repeats its stream info (per PMT), so ffprobe prints a
/// row per repetition; MP4 prints one. Taking the first line is correct for both
/// (every row reports the same per-stream value here).
fn ffprobe_first_line(args: &[&str]) -> String {
    let output = Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(args)
        .output()
        .expect("spawn ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("ffprobe stdout is UTF-8");
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Probe a single `stream=<entry>` field of the first video stream.
fn probe_field(path: &Path, entry: &str) -> String {
    ffprobe_first_line(&[
        "-select_streams",
        "v:0",
        "-show_entries",
        &format!("stream={entry}"),
        "-of",
        "default=nw=1:nk=1",
        path.to_str().unwrap(),
    ])
}

/// The `codec_name`, `width`, `height` of the first video stream, plus the
/// number of decodable frames, as ffprobe reports them.
fn probe_video(path: &Path) -> (String, u32, u32, u64) {
    let codec = probe_field(path, "codec_name");
    let width: u32 = probe_field(path, "width").parse().unwrap();
    let height: u32 = probe_field(path, "height").parse().unwrap();
    let frames = ffprobe_first_line(&[
        "-select_streams",
        "v:0",
        "-count_frames",
        "-show_entries",
        "stream=nb_read_frames",
        "-of",
        "default=nw=1:nk=1",
        path.to_str().unwrap(),
    ])
    .parse()
    .unwrap();
    (codec, width, height, frames)
}

#[test]
fn select_encoder_resolves_lgpl_mpeg2video_and_encodes_a_probeable_ts() {
    // The LGPL software codec always resolves in any FFmpeg build.
    let name = select_encoder(VideoCodec::Mpeg2Video).expect("mpeg2video must resolve");
    assert_eq!(name, "mpeg2video", "LGPL software encoder selected");

    let dir = TempDir::new().unwrap();
    let out = dir.path().join("mpeg2.ts");
    let packets = encode_to_container(name, "mpegts", &out);
    assert!(packets > 0, "encoder produced packets");
    assert!(out.exists() && std::fs::metadata(&out).unwrap().len() > 0);

    let (codec, width, height, frames) = probe_video(&out);
    assert_eq!(codec, "mpeg2video", "ffprobe confirms the codec");
    assert_eq!((width, height), (W, H), "ffprobe confirms the geometry");
    assert_eq!(
        frames,
        u64::try_from(FRAMES).unwrap(),
        "ffprobe counts every encoded frame"
    );
}

#[cfg(not(feature = "gpl-codecs"))]
#[cfg(not(feature = "cuda"))]
#[test]
fn lgpl_clean_build_cannot_select_an_h264_encoder() {
    // Without gpl-codecs / cuda the build genuinely cannot encode H.264/H.265:
    // selection returns None rather than reaching a GPL/proprietary encoder.
    assert_eq!(select_encoder(VideoCodec::H264), None);
    assert_eq!(select_encoder(VideoCodec::H265), None);
}

#[cfg(feature = "gpl-codecs")]
#[test]
fn select_encoder_resolves_libx264_and_encodes_a_probeable_mp4() {
    // With gpl-codecs (and no GPU here) H.264 resolves to the GPL software
    // encoder libx264. The linked FFmpeg in the dev image has it; if a build
    // lacked it, selection would correctly fall through to None and skip.
    let Some(name) = select_encoder(VideoCodec::H264) else {
        panic!("gpl-codecs build must resolve an H.264 encoder");
    };
    assert_eq!(
        name, "libx264",
        "with gpl-codecs and no GPU, H.264 selects libx264"
    );

    let dir = TempDir::new().unwrap();

    // (a) MPEG-TS: an exact, edit-list-free frame count. libx264 is 1-in-1-out,
    //     so every one of the `FRAMES` input frames must decode back out.
    let ts = dir.path().join("h264.ts");
    let ts_packets = encode_to_container(name, "mpegts", &ts);
    assert_eq!(
        ts_packets,
        u64::try_from(FRAMES).unwrap(),
        "libx264 is 1-in-1-out at the packet level"
    );
    let (codec, width, height, frames) = probe_video(&ts);
    assert_eq!(codec, "h264", "ffprobe confirms an H.264 stream");
    assert_eq!((width, height), (W, H), "ffprobe confirms the geometry");
    assert_eq!(
        frames,
        u64::try_from(FRAMES).unwrap(),
        "ffprobe counts every encoded frame in the MPEG-TS"
    );

    // (b) MP4: prove the same encode also produces a valid, playable .mp4 H.264
    //     stream of the right geometry. (The `-count_frames` value for B-frame
    //     H.264 in MP4 can read one short due to the edit-list presentation
    //     trim, a container quirk — exact frame counting is asserted on the TS
    //     above; here we assert codec/geometry and that frames decode.)
    let mp4 = dir.path().join("h264.mp4");
    let mp4_packets = encode_to_container(name, "mp4", &mp4);
    assert_eq!(
        mp4_packets,
        u64::try_from(FRAMES).unwrap(),
        "libx264 -> mp4 is 1-in-1-out at the packet level"
    );
    let (mp4_codec, mp4_w, mp4_h, mp4_frames) = probe_video(&mp4);
    assert_eq!(mp4_codec, "h264", "ffprobe confirms an H.264 mp4 stream");
    assert_eq!((mp4_w, mp4_h), (W, H), "ffprobe confirms the mp4 geometry");
    assert!(
        mp4_frames >= u64::try_from(FRAMES).unwrap() - 1,
        "the mp4 decodes essentially every frame (got {mp4_frames})"
    );
}
