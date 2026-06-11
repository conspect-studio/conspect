//! Finalize-on-error test for [`FileSink`] (the `ffmpeg` feature).
//!
//! A mid-run source error must still leave the partially-written container
//! structurally valid — the muxer trailer is written best-effort **before** the
//! error propagates, so a player can open and decode the partial output rather
//! than choke on a container that is missing its trailer.
//!
//! This uses an **MP4** output on purpose: MP4's trailer (the `moov` atom) is
//! load-bearing, so a sink that skips `finish()` on the error path leaves a file
//! `ffprobe` rejects with "moov atom not found". (MPEG-TS has no real trailer
//! and would mask the bug, so the segment-sink finalize behaviour is covered by
//! an in-crate unit test against a private finalize counter instead.)
//!
//! Licensing: the source clip and the re-encode both use `mpeg2video`, an LGPL
//! software codec already in `FFmpeg` — never x264/x265 (which would be GPL).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;
use multiview_ffmpeg::{DecodedVideoFrame, StreamVideoDecoder};
use multiview_output::sink::{EncodeConfig, FileSink, VideoFrameSource};
use multiview_output::{Error, Result};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

/// Generate a tiny `mpeg2video` MPEG-TS clip with the `ffmpeg` CLI.
fn generate_clip(path: &Path, seconds: u32, fps: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={WIDTH}x{HEIGHT}:rate={fps}:duration={seconds}"),
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-g",
            &fps.to_string(),
            "-keyint_min",
            &fps.to_string(),
            "-sc_threshold",
            "0",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(path.exists(), "clip was not written");
}

/// A [`VideoFrameSource`] that decodes a file to NV12 frames via `multiview-ffmpeg`.
struct DecodeSource {
    input: ffmpeg::format::context::Input,
    decoder: StreamVideoDecoder,
    stream_index: usize,
    drained: bool,
}

impl DecodeSource {
    fn open(path: &Path) -> Self {
        let input = ffmpeg::format::input(&path).expect("open input container");
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("input has a video stream");
        let stream_index = stream.index();
        let params = stream.parameters();
        let time_base = multiview_ffmpeg::from_ff_rational(stream.time_base());
        let decoder = StreamVideoDecoder::new(params, time_base).expect("build stream decoder");
        Self {
            input,
            decoder,
            stream_index,
            drained: false,
        }
    }
}

impl VideoFrameSource for DecodeSource {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        loop {
            if let Some(frame) = self
                .decoder
                .receive_frame()
                .map_err(|e| Error::Output(e.to_string()))?
            {
                return Ok(Some(frame));
            }
            if self.drained {
                return Ok(None);
            }
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        self.decoder
                            .send_packet(&packet)
                            .map_err(|e| Error::Output(e.to_string()))?;
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder
                        .send_eof()
                        .map_err(|e| Error::Output(e.to_string()))?;
                    self.drained = true;
                }
                Err(other) => return Err(Error::Output(other.to_string())),
            }
        }
    }
}

/// A source that yields `before_err` frames from an inner source, then errors —
/// modelling a mid-run input failure (`?` returns) while frames are in flight.
struct FailAfter {
    inner: DecodeSource,
    remaining: usize,
}

impl FailAfter {
    fn new(inner: DecodeSource, before_err: usize) -> Self {
        Self {
            inner,
            remaining: before_err,
        }
    }
}

impl VideoFrameSource for FailAfter {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        if self.remaining == 0 {
            return Err(Error::Output("injected mid-run source failure".to_owned()));
        }
        self.remaining -= 1;
        self.inner.next_frame()
    }
}

/// Probe a container with `ffprobe`; returns `true` only if it opens as a valid
/// container reporting a video stream with no errors. A trailer-less MP4 fails
/// here with "moov atom not found", so this is the real structural-validity
/// check the fix must satisfy.
fn ffprobe_opens_cleanly(path: &Path) -> bool {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe CLI");
    let stderr = String::from_utf8_lossy(&output.stderr);
    output.status.success()
        && stderr.trim().is_empty()
        && String::from_utf8_lossy(&output.stdout).contains("video")
}

/// Count the decodable video frames in a container by re-decoding it.
fn decode_frame_count(path: &Path) -> usize {
    let mut source = DecodeSource::open(path);
    let mut count = 0;
    while source.next_frame().expect("decode frame").is_some() {
        count += 1;
    }
    count
}

fn config(fps: u32, gop: u32) -> EncodeConfig {
    let mut cfg = EncodeConfig::mpeg2(WIDTH, HEIGHT);
    cfg.cadence = Rational::new(i64::from(fps), 1);
    cfg.gop = gop;
    cfg
}

#[test]
fn file_sink_finalizes_mp4_container_when_source_errors_mid_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 3, 30);

    // MP4: the trailer (moov atom) is mandatory, so skipping finish() on the
    // error path yields a container ffprobe rejects.
    let out = dir.path().join("program.mp4");
    let sink = FileSink::new(config(30, 30), &out);
    // Yield ~1.5 GOPs of frames, then fail mid-run.
    let mut source = FailAfter::new(DecodeSource::open(&src), 45);
    let err = sink
        .run(&mut source)
        .expect_err("run must surface the source error");
    assert!(
        matches!(err, Error::Output(_)),
        "the injected source error must propagate, got {err:?}"
    );

    // Despite the error, the partial container must be finalized (moov written)
    // so it opens cleanly and decodes its already-written frames.
    assert!(out.exists(), "no partial container was written");
    assert!(
        out.metadata().expect("stat output").len() > 0,
        "partial container is empty"
    );
    assert!(
        ffprobe_opens_cleanly(&out),
        "partial MP4 is missing its moov trailer (ffprobe could not open it cleanly)"
    );
    assert!(
        decode_frame_count(&out) > 0,
        "partial container decoded to zero frames"
    );
}
