//! Push transport + GPL (x264) sink tests for the real output sinks.
//!
//! Two concerns:
//!
//! * **`PushSink`** (always under `ffmpeg`) — the protocol → libav muxer mapping
//!   is asserted exactly, and a push to an unreachable peer surfaces a typed
//!   [`Error`] (never a hang or panic). A live push needs a peer, so the
//!   happy-path connect is not run in CI; the construction, mapping, and
//!   graceful-failure paths are.
//! * **GPL `FileSink`** (under `gpl-codecs`) — a synthetic NV12 program is
//!   encoded with `libx264` and muxed to a real `.ts`, then `ffprobe` confirms an
//!   `h264` stream of the right geometry and frame count. The only GPL codec
//!   touched is behind the `gpl-codecs` feature, never the default build.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[cfg(feature = "gpl-codecs")]
use std::path::Path;
#[cfg(feature = "gpl-codecs")]
use std::process::Command;

use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;
use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::DecodedVideoFrame;
use multiview_output::sink::{EncodeConfig, PushProtocol, PushSink, VideoFrameSource};
use multiview_output::{Error, Result};

const W: u32 = 128;
const H: u32 = 96;

/// A finite [`VideoFrameSource`] of `count` flat-gray NV12 frames synthesized in
/// process (no input file). Mirrors what the compositor's program output hands a
/// sink: NV12 frames whose own timestamps the sink ignores and re-stamps.
struct GrayNv12Source {
    remaining: u32,
}

impl GrayNv12Source {
    fn new(count: u32) -> Self {
        Self { remaining: count }
    }
}

impl VideoFrameSource for GrayNv12Source {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        // NV12: a full-size Y plane (mid-gray) then a half-height interleaved UV
        // plane (neutral 128). `Video::new` allocates both planes.
        let mut frame = Video::new(Pixel::NV12, W, H);
        for p in 0..frame.planes() {
            for byte in frame.data_mut(p).iter_mut() {
                *byte = 128;
            }
        }
        let meta = FrameMeta {
            pts: MediaTime::ZERO,
            width: W,
            height: H,
            format: PixelFormat::Nv12,
            // All four axes default to Unspecified (the detect step never
            // guesses — invariant #8); the sink re-stamps and tags downstream.
            color: ColorInfo::default(),
        };
        Ok(Some(DecodedVideoFrame {
            frame,
            meta,
            raw_pts: None,
            a53_cc: None,
        }))
    }
}

fn ts_config(codec: &str, fps: i64, gop: u32) -> EncodeConfig {
    EncodeConfig {
        codec_name: codec.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        cadence: Rational::new(fps, 1),
        gop,
        bit_rate: 800_000,
        audio: None,
        cuda_ordinal: None,
    }
}

/// `ffprobe` the first non-empty line of a single `stream=<entry>` field.
#[cfg(feature = "gpl-codecs")]
fn probe_field(path: &Path, entry: &str) -> String {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            &format!("stream={entry}"),
            "-of",
            "default=nw=1:nk=1",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("spawn ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

#[cfg(feature = "gpl-codecs")]
fn probe_frame_count(path: &Path) -> u64 {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=nw=1:nk=1",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("spawn ffprobe");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("0")
        .parse()
        .unwrap()
}

#[test]
fn push_protocol_maps_to_the_expected_libav_muxer() {
    // The protocol fixes the container muxer the same way an extension fixes a
    // file's; this mapping is the contract a deploy relies on.
    assert_eq!(PushProtocol::Rtmp.muxer_name(), "flv");
    assert_eq!(PushProtocol::Srt.muxer_name(), "mpegts");
    assert_eq!(PushProtocol::UdpTs.muxer_name(), "mpegts");
    assert_eq!(PushProtocol::Rtsp.muxer_name(), "rtsp");

    let sink = PushSink::new(
        ts_config("mpeg2video", 25, 25),
        PushProtocol::Rtmp,
        "rtmp://example.invalid/live/stream",
    );
    assert_eq!(sink.protocol(), PushProtocol::Rtmp);
    assert_eq!(sink.muxer_name(), "flv");
    assert_eq!(sink.url(), "rtmp://example.invalid/live/stream");
}

#[test]
fn push_to_unreachable_peer_fails_gracefully() {
    // A push needs a listening peer; with none, opening the muxer must surface a
    // typed Error (libav connect failure) rather than hang or panic. Port 1 on
    // the IPv6 loopback is reliably refused. UDP is connectionless so we use a
    // TCP-based transport (RTMP) which fails fast on a refused connection.
    let sink = PushSink::new(
        ts_config("mpeg2video", 25, 25),
        PushProtocol::Rtmp,
        "rtmp://[::1]:1/live/none",
    );
    let mut source = GrayNv12Source::new(5);
    match sink.run(&mut source) {
        Ok(_) => panic!("a push to an unreachable peer must not succeed"),
        Err(Error::Output(msg)) => {
            assert!(!msg.is_empty(), "the connect error carries a message");
        }
        Err(other) => panic!("expected Error::Output, got {other:?}"),
    }
}

#[cfg(feature = "gpl-codecs")]
#[test]
fn file_sink_encodes_an_h264_ts_with_libx264() {
    use multiview_output::sink::FileSink;

    const FRAMES: u32 = 30;
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("program_h264.ts");

    // libx264 is the GPL H.264 software encoder (behind gpl-codecs only).
    let sink = FileSink::new(ts_config("libx264", 25, 12), &out);
    let mut source = GrayNv12Source::new(FRAMES);
    let stats = sink.run(&mut source).expect("file sink run");

    assert!(out.exists(), "no output container written");
    assert!(
        out.metadata().expect("stat output").len() > 0,
        "output container is empty"
    );
    assert_eq!(
        stats.packets,
        u64::from(FRAMES),
        "libx264 is 1-in-1-out at the packet level"
    );
    assert!(stats.keyframes > 0, "no keyframe was encoded");

    // ffprobe confirms a real, playable H.264 MPEG-TS of the right shape.
    assert_eq!(probe_field(&out, "codec_name"), "h264");
    assert_eq!(probe_field(&out, "width").parse::<u32>().unwrap(), W);
    assert_eq!(probe_field(&out, "height").parse::<u32>().unwrap(), H);
    assert_eq!(
        probe_frame_count(&out),
        u64::from(FRAMES),
        "ffprobe counts every encoded frame"
    );
}
