//! Demuxer integration tests against a self-contained, CLI-generated clip.
//!
//! Gated behind the `ffmpeg` feature. The clip is generated at test time with
//! the **LGPL** `ffv1` video codec and `flac` audio (both in-tree, never
//! x264/x265), so the suite carries no media and stays LGPL-clean.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::Demuxer;
use tempfile::TempDir;

const W: u32 = 320;
const H: u32 = 240;
const RATE: u32 = 25;
const SECONDS: u32 = 1;

/// Generate a 1-second A/V clip (`ffv1` video + `flac` audio) into `dir`.
fn generate_av_clip(dir: &Path) -> PathBuf {
    let out = dir.join("av.mkv");
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
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            &SECONDS.to_string(),
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "flac",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate clip");
    assert!(out.exists());
    out
}

#[test]
fn lists_video_and_audio_streams_with_geometry_and_timebase() {
    let dir = TempDir::new().unwrap();
    let clip = generate_av_clip(dir.path());

    let demux = Demuxer::open(&clip).expect("open container");
    let streams = demux.streams();
    assert_eq!(streams.len(), 2, "one video + one audio stream");

    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("a video stream");
    assert_eq!(video.width, W, "video width from codec params");
    assert_eq!(video.height, H, "video height from codec params");
    // The stream time-base must be a usable (non-zero-denominator) rational.
    assert!(video.time_base.is_valid(), "video time-base must be valid");

    let audio = streams
        .iter()
        .find(|s| s.kind == MediaKind::Audio)
        .expect("an audio stream");
    assert_eq!(audio.sample_rate, 48_000, "audio sample rate");
    assert!(audio.channels >= 1, "at least one audio channel");
}

#[test]
fn best_video_stream_resolves_and_reads_keyframe_first() {
    let dir = TempDir::new().unwrap();
    let clip = generate_av_clip(dir.path());

    let mut demux = Demuxer::open(&clip).expect("open container");
    let vidx = demux
        .best_stream(MediaKind::Video)
        .expect("a best video stream");

    // The first packet of the video stream must exist and be a keyframe (ffv1
    // is all-intra, so every frame is a keyframe).
    let first = demux
        .read_packet_for(vidx)
        .expect("read without error")
        .expect("at least one video packet");
    assert_eq!(first.stream_index, vidx);
    assert!(first.size() > 0, "packet must carry payload");
    assert!(first.is_key(), "first ffv1 packet is a keyframe");
}

#[test]
fn reads_expected_total_video_packet_count() {
    let dir = TempDir::new().unwrap();
    let clip = generate_av_clip(dir.path());

    let mut demux = Demuxer::open(&clip).expect("open container");
    let vidx = demux.best_stream(MediaKind::Video).unwrap();

    let mut count = 0_u32;
    while let Some(pkt) = demux.read_packet().expect("read without error") {
        if pkt.stream_index == vidx {
            count += 1;
        }
    }
    // testsrc at 25 fps for exactly 1 second yields 25 video frames/packets.
    assert_eq!(count, RATE * SECONDS, "video packet count");
}

#[test]
fn open_missing_file_is_typed_error_not_panic() {
    let missing = Path::new("/nonexistent/multiview-ffmpeg/missing.mkv");
    match Demuxer::open(missing) {
        Ok(_) => panic!("opening a missing file must fail"),
        Err(err) => assert!(
            err.to_string().contains("missing.mkv"),
            "error names the path: {err}"
        ),
    }
}

#[test]
fn demuxer_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Demuxer>();
}
