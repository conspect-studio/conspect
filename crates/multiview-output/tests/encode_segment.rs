//! End-to-end encode-once-mux-many tests for the real output sinks (the
//! `ffmpeg` feature).
//!
//! These are self-contained: each generates a tiny test clip with the `ffmpeg`
//! CLI (lavfi `testsrc`) into a tempdir, decodes it to NV12 frames, then drives
//! the crate's [`FileSink`]/[`SegmentSink`] to encode (LGPL `mpeg2video`) and
//! mux/segment. They assert real artifacts: a playable container file, segment
//! files that exist, a playlist that references exactly those segments, and that
//! a written segment re-opens and decodes.
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

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;
use multiview_ffmpeg::{DecodedVideoFrame, StreamVideoDecoder};
use multiview_output::sink::{EncodeConfig, FileSink, SegmentSink, VideoFrameSource};
use multiview_output::{Error, Result};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

/// Generate a tiny `mpeg2video` MPEG-TS clip with the `ffmpeg` CLI. Skips (via a
/// returned `None`) only if the CLI is entirely absent; the dev/CI image has it.
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

/// A [`VideoFrameSource`] that decodes a file to NV12 frames via `multiview-ffmpeg`'s
/// [`StreamVideoDecoder`].
///
/// The decoder needs the stream's libav `Parameters`, which `multiview-ffmpeg`'s
/// current safe API does not surface from its `Demuxer`; this test scaffold
/// therefore opens the container with `ffmpeg-next` directly to obtain them.
/// This is **test-only** plumbing — it writes no `unsafe` and performs no FFI,
/// it only names `ffmpeg-next`'s safe `Parameters`/`Input` value types to bridge
/// demux → decode. Production code in `src/` uses only `multiview-ffmpeg`'s safe
/// wrappers.
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
            // First, try to pull an already-decoded frame.
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
            // Feed the next packet from the video stream, or EOF the decoder.
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
fn file_sink_encodes_a_decodable_container() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 1, 30);
    let frames_in = decode_frame_count(&src);
    assert!(frames_in > 0, "source decoded to zero frames");

    let out = dir.path().join("program.ts");
    let sink = FileSink::new(config(30, 30), &out);
    let mut source = DecodeSource::open(&src);
    let stats = sink.run(&mut source).expect("file sink run");

    // Real artifact: a non-empty container with at least one keyframe.
    assert!(out.exists(), "no output container written");
    assert!(
        out.metadata().expect("stat output").len() > 0,
        "output container is empty"
    );
    assert!(stats.packets > 0, "no packets were encoded");
    assert!(stats.keyframes > 0, "no keyframe was encoded");

    // The re-encoded container re-opens and decodes to a comparable frame count
    // (mpeg2video is 1-in-1-out, so the count must match exactly).
    let frames_out = decode_frame_count(&out);
    assert_eq!(
        frames_out, frames_in,
        "re-encoded container decoded to a different frame count"
    );
}

#[test]
fn segment_sink_writes_segments_referenced_by_the_playlist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    // 2 seconds at 30 fps with a 1-second GOP => at least two keyframes => at
    // least two GOP-aligned segments.
    generate_clip(&src, 2, 30);

    let result = {
        let sink = SegmentSink::new(config(30, 30), dir.path(), "seg");
        let mut source = DecodeSource::open(&src);
        sink.run(&mut source).expect("segment sink run")
    };

    // We must have produced multiple GOP-aligned segments.
    assert!(
        result.segments.len() >= 2,
        "expected >= 2 segments, got {}",
        result.segments.len()
    );

    // Every segment file exists, is non-empty, and is named in the playlist.
    let playlist_text = result.playlist.render();
    for seg in &result.segments {
        assert!(seg.exists(), "segment {seg:?} was not written");
        assert!(
            seg.metadata().expect("stat segment").len() > 0,
            "segment {seg:?} is empty"
        );
        let name = seg.file_name().unwrap().to_str().unwrap();
        assert!(
            playlist_text.contains(name),
            "playlist does not reference segment {name}\n--- playlist ---\n{playlist_text}"
        );
    }

    // The playlist is a finished, well-formed HLS media playlist.
    assert!(playlist_text.starts_with("#EXTM3U"));
    assert!(playlist_text.contains("#EXT-X-TARGETDURATION:"));
    assert!(playlist_text.contains("#EXT-X-ENDLIST"));
    // One EXTINF per segment.
    let extinf = playlist_text.matches("#EXTINF:").count();
    assert_eq!(
        extinf,
        result.segments.len(),
        "playlist EXTINF count must equal the segment count"
    );

    // Re-open the FIRST segment and decode it: each segment must be an
    // independently decodable MPEG-TS file beginning on a keyframe.
    let first: &PathBuf = &result.segments[0];
    let decoded = decode_frame_count(first);
    assert!(
        decoded > 0,
        "first segment {first:?} did not decode any frame"
    );
}

#[test]
fn segment_sink_keeps_one_encode_for_many_segments() {
    // Encode-once-mux-many (invariant #7): the stats report a single encoded
    // packet stream whose keyframe count equals the segment count — i.e. each
    // segment is one GOP of the SAME encode, not a separate re-encode.
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 2, 30);

    let sink = SegmentSink::new(config(30, 30), dir.path(), "g");
    let mut source = DecodeSource::open(&src);
    let result = sink.run(&mut source).expect("segment sink run");

    let keyframes = usize::try_from(result.stats.keyframes).expect("keyframe count fits usize");
    assert_eq!(
        keyframes,
        result.segments.len(),
        "each segment must correspond to exactly one keyframe of the single encode"
    );
    assert!(
        result.stats.packets >= result.stats.keyframes,
        "packet count must be at least the keyframe count"
    );
}
