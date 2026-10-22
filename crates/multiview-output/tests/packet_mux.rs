//! Packet-fed mux-only sink tests (the `ffmpeg` feature).
//!
//! Layer 2 of encode-once-mux-many (ADR-0026 / invariant #7): the cli encodes
//! the canvas ONCE, then fans the SAME coded packets to N mux-only sinks. These
//! tests feed canned encoded packets (encoded once here with `VideoEncoder`)
//! through a [`PacketMuxSink`] and assert:
//!
//! * `PacketMuxSink::file` muxes a valid, decodable container with the right
//!   packet count;
//! * `PacketMuxSink::segment` splits on keyframe-flagged packets into N segments
//!   matching the GOP boundaries and writes a referencing playlist;
//! * a mid-stream `PacketSource` error still finalizes the trailer (MP4, where
//!   the moov atom is load-bearing); and
//! * the headline property: ONE encode fanned to a File AND a Segment sink
//!   yields the same decoded frame count in both (encode-once-mux-many).
//!
//! Licensing: encodes use `mpeg2video`, an LGPL software codec already in
//! `FFmpeg` — never x264/x265 (which would be GPL).
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
use multiview_ffmpeg::{
    DecodedVideoFrame, EncodedPacket, ScaleSpec, Scaler, StreamCodecParameters, StreamVideoDecoder,
    VideoEncodeTarget, VideoEncoder,
};
use multiview_output::sink::{PacketMuxOutcome, PacketMuxSink, PacketSource};
use multiview_output::{Error, Result};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

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
}

/// A test-only decode source over a file (matches the existing sink tests).
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

    fn next_frame(&mut self) -> Option<DecodedVideoFrame> {
        loop {
            if let Some(frame) = self.decoder.receive_frame().expect("decode frame") {
                return Some(frame);
            }
            if self.drained {
                return None;
            }
            let mut packet = ffmpeg::codec::packet::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        self.decoder.send_packet(&packet).expect("send packet");
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof().expect("send eof");
                    self.drained = true;
                }
                Err(other) => panic!("decode read error: {other}"),
            }
        }
    }
}

fn target(fps: u32, gop: u32) -> VideoEncodeTarget {
    VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, i64::from(fps)),
        bit_rate: 2_000_000,
        gop,
        cuda_device: None,
    }
}

/// Encode the whole `src` clip ONCE into a vector of [`EncodedPacket`]s, plus the
/// `Send` codec-params snapshot and the encoder time-base — exactly the trio the
/// cli's consumer thread would hand to its mux sinks.
fn encode_once(
    src: &Path,
    fps: u32,
    gop: u32,
) -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    let mut encoder = VideoEncoder::new(&target(fps, gop)).expect("open encoder");
    let time_base = encoder.time_base();
    let params = StreamCodecParameters::from_encoder(&encoder);

    let mut decode = DecodeSource::open(src);
    let mut scaler: Option<Scaler> = None;
    let mut packets = Vec::new();
    let mut tick: i64 = 0;
    while let Some(frame) = decode.next_frame() {
        let mut video: Video = frame.frame;
        if video.format() != Pixel::YUV420P {
            let s = ScaleSpec::new(video.format(), video.width(), video.height());
            let d = ScaleSpec::new(Pixel::YUV420P, video.width(), video.height());
            let sc = scaler.get_or_insert_with(|| Scaler::new(s, d).expect("scaler"));
            video = sc.run(&video).expect("scale");
        }
        video.set_pts(Some(tick));
        encoder.send_frame(&video).expect("send frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv") {
            packets.push(EncodedPacket::from_packet(pkt));
        }
        tick += 1;
    }
    encoder.send_eof().expect("eof");
    while let Some(pkt) = encoder.receive_packet().expect("drain") {
        packets.push(EncodedPacket::from_packet(pkt));
    }
    (packets, params, time_base)
}

/// A [`PacketSource`] over a pre-encoded vector (drains front-to-back).
struct VecPackets {
    packets: std::vec::IntoIter<EncodedPacket>,
}

impl VecPackets {
    fn new(packets: Vec<EncodedPacket>) -> Self {
        Self {
            packets: packets.into_iter(),
        }
    }
}

impl PacketSource for VecPackets {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.next())
    }
}

/// A [`PacketSource`] that yields `before_err` packets, then errors mid-stream.
struct FailAfter {
    inner: VecPackets,
    remaining: usize,
}

impl PacketSource for FailAfter {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        if self.remaining == 0 {
            return Err(Error::Output(
                "injected mid-stream packet failure".to_owned(),
            ));
        }
        self.remaining -= 1;
        self.inner.next_packet()
    }
}

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
        .expect("spawn ffprobe");
    let stderr = String::from_utf8_lossy(&output.stderr);
    output.status.success()
        && stderr.trim().is_empty()
        && String::from_utf8_lossy(&output.stdout).contains("video")
}

fn decode_frame_count(path: &Path) -> usize {
    let mut source = DecodeSource::open(path);
    let mut count = 0;
    while source.next_frame().is_some() {
        count += 1;
    }
    count
}

fn keyframe_count(packets: &[EncodedPacket]) -> usize {
    packets.iter().filter(|p| p.is_keyframe()).count()
}

#[test]
fn file_packet_sink_muxes_a_decodable_container() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 1, 30);
    let frames_in = decode_frame_count(&src);
    assert!(frames_in > 0);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    let packet_total = u64::try_from(packets.len()).expect("packet count fits u64");
    assert!(packet_total > 0);

    let out = dir.path().join("program.ts");
    let sink = PacketMuxSink::file(&out);
    let mut source = VecPackets::new(packets);
    let outcome = sink
        .run(&mut source, &params, time_base)
        .expect("packet file sink run");

    let stats = match outcome {
        PacketMuxOutcome::Single(stats) => stats,
        PacketMuxOutcome::Segment(_) => panic!("file sink must return a Single outcome"),
    };
    assert_eq!(
        stats.packets, packet_total,
        "every fanned packet must be muxed"
    );
    assert!(stats.keyframes > 0, "at least one keyframe was muxed");

    assert!(out.exists() && out.metadata().expect("stat").len() > 0);
    assert_eq!(
        decode_frame_count(&out),
        frames_in,
        "muxed container decodes to the same frame count (mpeg2video is 1-in-1-out)"
    );
}

#[test]
fn segment_packet_sink_splits_on_keyframes_and_writes_a_playlist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    // 2s @ 30fps, 1s GOP => >= 2 keyframes => >= 2 segments.
    generate_clip(&src, 2, 30);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    let expected_segments = keyframe_count(&packets);
    assert!(
        expected_segments >= 2,
        "test needs >= 2 keyframes, got {expected_segments}"
    );

    let sink = PacketMuxSink::segment(dir.path(), "seg");
    let mut source = VecPackets::new(packets);
    let outcome = sink
        .run(&mut source, &params, time_base)
        .expect("packet segment sink run");
    let result = match outcome {
        PacketMuxOutcome::Segment(result) => result,
        PacketMuxOutcome::Single(_) => panic!("segment sink must return a Segment outcome"),
    };

    // Splits on keyframes: one segment per keyframe-flagged packet.
    assert_eq!(
        result.segments.len(),
        expected_segments,
        "segment count must equal the keyframe count (GOP boundaries)"
    );
    let keyframes = usize::try_from(result.stats.keyframes).expect("fits");
    assert_eq!(keyframes, expected_segments, "stats keyframes == segments");

    let playlist = result.playlist.render();
    assert!(playlist.starts_with("#EXTM3U"));
    assert!(playlist.contains("#EXT-X-ENDLIST"));
    assert_eq!(
        playlist.matches("#EXTINF:").count(),
        result.segments.len(),
        "one EXTINF per segment"
    );
    for seg in &result.segments {
        assert!(seg.exists() && seg.metadata().expect("stat").len() > 0);
        let name = seg.file_name().unwrap().to_str().unwrap();
        assert!(playlist.contains(name), "playlist references {name}");
    }
    // Each segment is independently decodable.
    assert!(decode_frame_count(&result.segments[0]) > 0);
}

#[test]
fn segment_live_publishes_rolling_playlist_bounded_window_and_prunes() {
    // HLS-0/1 (ADR-0032): the LIVE segment sink writes the `.m3u8` on disk on
    // every closed segment (not once-at-finalize), bounds the segment window, and
    // prunes the evicted `.ts` files. This drives the full encoded-packet → live
    // segment-muxer → rolling-playlist path end to end (the `LivePlaylist` unit
    // test covers the pure driver; this proves the wiring through the real muxer +
    // atomic rename).
    const WINDOW: usize = 4;

    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    // 6s @ 30fps, 1s GOP => ~6 keyframes => ~6 GOP-aligned segments, comfortably
    // more than the window so eviction + pruning are exercised.
    generate_clip(&src, 6, 30);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    let total_segments = keyframe_count(&packets);
    assert!(
        total_segments > WINDOW,
        "test needs more keyframes ({total_segments}) than the window ({WINDOW}) to exercise eviction"
    );

    let playlist_path = dir.path().join("multiview.m3u8");
    // No epoch is published into the cell here, so the manifest carries no
    // PDT tags -- byte-identical playlist behaviour to before DEV-C1.
    let sink = PacketMuxSink::segment_live(
        dir.path(),
        "seg",
        &playlist_path,
        WINDOW,
        multiview_output::SharedEpoch::new(),
    );
    let mut source = VecPackets::new(packets);
    let outcome = sink
        .run(&mut source, &params, time_base)
        .expect("live segment sink run");
    let result = match outcome {
        PacketMuxOutcome::Segment(result) => result,
        PacketMuxOutcome::Single(_) => panic!("segment sink must return a Segment outcome"),
    };

    // The on-disk playlist exists (written by the rolling driver, not the cli's
    // finalize-time write), and was published throughout the run.
    assert!(
        playlist_path.exists(),
        "the live `.m3u8` must be written to disk by the rolling driver"
    );
    let text = std::fs::read_to_string(&playlist_path).expect("read live playlist");

    // It lists exactly the window of most-recent segments. A segment URI is a
    // non-empty, non-tag line (every directive starts with `#`).
    let listed: Vec<&str> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        listed.len(),
        WINDOW,
        "the live playlist must list exactly the window size; playlist was:\n{text}"
    );

    // EXT-X-MEDIA-SEQUENCE advanced as oldest segments were evicted.
    let expected_msn = total_segments - WINDOW;
    assert!(
        text.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{expected_msn}")),
        "EXT-X-MEDIA-SEQUENCE must advance to N-window = {expected_msn}; playlist was:\n{text}"
    );

    // The run finished (finite source), so the finalized playlist carries ENDLIST.
    assert!(
        text.contains("#EXT-X-ENDLIST"),
        "a finalized live run must carry #EXT-X-ENDLIST; playlist was:\n{text}"
    );

    // Disk is bounded: exactly `window` `seg*.ts` files remain (the rest pruned).
    let on_disk: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(std::ffi::OsStr::to_str) == Some("ts")
                && p.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|n| n.starts_with("seg"))
        })
        .collect();
    assert_eq!(
        on_disk.len(),
        WINDOW,
        "disk must be bounded to the window: {} seg*.ts files remain, expected {WINDOW}",
        on_disk.len()
    );

    // No leftover `.tmp` files: every segment was renamed into place atomically.
    let tmp_left = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .any(|p| p.extension().and_then(std::ffi::OsStr::to_str) == Some("tmp"));
    assert!(
        !tmp_left,
        "no `.tmp` segment/playlist file must linger after atomic publish"
    );

    // The monotonic counter never reused an index: the last listed segment names a
    // higher index than the window count (eviction did not recycle a name).
    let last = listed.last().expect("a windowed segment");
    let last_idx: usize = last
        .trim_start_matches("seg")
        .trim_end_matches(".ts")
        .parse()
        .expect("segment index parses");
    assert_eq!(
        last_idx,
        total_segments - 1,
        "the newest segment index must be monotonic (N-1), never recycled from the pruned set"
    );

    // Every still-listed segment file exists, is non-empty, and decodes.
    for name in &listed {
        let seg = dir.path().join(name);
        assert!(
            seg.exists() && seg.metadata().expect("stat").len() > 0,
            "{name} present"
        );
    }
    assert!(
        decode_frame_count(&dir.path().join(listed[0])) > 0,
        "a windowed segment must be independently decodable"
    );
    // Sanity: the run still reports the full segment set it produced.
    assert_eq!(
        result.segments.len(),
        total_segments,
        "the run report must record every segment produced, not just the windowed ones"
    );
}

#[test]
fn packet_sink_finalizes_mp4_when_source_errors_mid_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 2, 30);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    assert!(packets.len() > 20, "need enough packets to fail partway");

    let out = dir.path().join("program.mp4");
    let sink = PacketMuxSink::file(&out);
    let mut source = FailAfter {
        inner: VecPackets::new(packets),
        remaining: 20,
    };
    let err = sink
        .run(&mut source, &params, time_base)
        .expect_err("mid-stream error must propagate");
    assert!(matches!(err, Error::Output(_)), "got {err:?}");

    // Despite the error, the moov trailer was written best-effort, so the
    // partial MP4 opens cleanly and decodes its already-muxed frames.
    assert!(out.exists() && out.metadata().expect("stat").len() > 0);
    assert!(
        ffprobe_opens_cleanly(&out),
        "partial MP4 missing its moov trailer"
    );
    assert!(
        decode_frame_count(&out) > 0,
        "partial MP4 decoded zero frames"
    );
}

#[test]
fn one_encode_fans_to_file_and_segment_with_equal_frame_counts() {
    // The headline encode-once-mux-many property: ONE encode, fan the SAME
    // packets to a File AND a Segment sink; both decode to the same frame count.
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 2, 30);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    assert!(keyframe_count(&packets) >= 2);

    // Two independent PacketSources over copies of the SAME encoded packets —
    // mirroring the cli fanning packet copies to two sink threads.
    let file_src_packets: Vec<EncodedPacket> = packets.clone();
    let seg_src_packets: Vec<EncodedPacket> = packets;

    let file_out = dir.path().join("program.ts");
    let file_sink = PacketMuxSink::file(&file_out);
    let mut file_source = VecPackets::new(file_src_packets);
    file_sink
        .run(&mut file_source, &params, time_base)
        .expect("file run");

    let seg_dir = dir.path().join("hls");
    std::fs::create_dir_all(&seg_dir).expect("mkdir hls");
    let seg_sink = PacketMuxSink::segment(&seg_dir, "seg");
    let mut seg_source = VecPackets::new(seg_src_packets);
    let seg_outcome = seg_sink
        .run(&mut seg_source, &params, time_base)
        .expect("segment run");
    let seg_result = match seg_outcome {
        PacketMuxOutcome::Segment(r) => r,
        PacketMuxOutcome::Single(_) => panic!("expected Segment"),
    };

    let file_frames = decode_frame_count(&file_out);
    let seg_frames: usize = seg_result
        .segments
        .iter()
        .map(|p| decode_frame_count(p))
        .sum();
    assert!(file_frames > 0 && seg_frames > 0);
    assert_eq!(
        file_frames, seg_frames,
        "one encode fanned to file + segments must decode to equal frame counts"
    );
}
