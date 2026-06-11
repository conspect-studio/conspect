//! Audio-aware packet-fed mux tests (the `ffmpeg` feature) — AUD-4 slice 2.
//!
//! Encode-once-mux-many fans the SAME coded packets to N mux-only sinks; with
//! program audio (AUD-4) each muxer registers a SECOND elementary stream and the
//! kind-tagged packets route to the matching `stream_index`. These tests build a
//! short run of real video packets (`mpeg2video`) and real audio packets
//! (native `aac`), tag them [`StreamKind::Video`]/[`StreamKind::Audio`], and
//! assert that:
//!
//! * a single-container [`PacketMuxSink::file`] fed both, via
//!   [`PacketMuxSink::run_av`] with a video + an audio [`MuxStream`], writes a
//!   container with EXACTLY one video and one audio stream (header-pinning,
//!   ADR-R005 §3.3); and
//! * a segmented (HLS) sink does the same per `.ts` segment.
//!
//! Licensing: `mpeg2video` + native `aac` are both LGPL — never x264/x265/fdk.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{Pixel, Sample};
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{
    AudioEncodeTarget, AudioEncoder, EncodedPacket, StreamCodecParameters, VideoEncodeTarget,
    VideoEncoder,
};
use multiview_output::sink::{MuxStream, PacketMuxOutcome, PacketMuxSink, PacketSource};
use multiview_output::Result;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const FPS: i64 = 30;
const SAMPLE_RATE: u32 = 48_000;
const VIDEO_FRAMES: i64 = 12;

/// Keyframe interval (GOP). With static content the encoder inserts a keyframe
/// only at each GOP boundary (no scene-change keyframes), so the segmenter cuts
/// `VIDEO_FRAMES / GOP` segments of `GOP` frames each — large enough for
/// `ffprobe` to analyze (a 1-frame TS segment can be too small to probe).
const GOP: u32 = 4;

/// Build a flat mid-gray planar-`yuv420p` frame stamped at `pts`. Static content
/// keeps the only keyframes at the GOP boundaries (deterministic segmentation).
fn gray_frame(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, WIDTH, HEIGHT);
    for value in frame.data_mut(0).iter_mut() {
        *value = 128;
    }
    for plane in 1..3 {
        for value in frame.data_mut(plane).iter_mut() {
            *value = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode `VIDEO_FRAMES` synthetic frames once with `mpeg2video`, returning the
/// kind-tagged video packets plus the codec-params snapshot + time-base a mux
/// sink registers its video stream from.
fn encode_video() -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    let target = VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, FPS),
        bit_rate: 2_000_000,
        gop: GOP,
        cuda_device: None,
    };
    let mut encoder = VideoEncoder::new(&target).expect("open video encoder");
    let time_base = encoder.time_base();
    let params = StreamCodecParameters::from_encoder(&encoder);
    let mut packets = Vec::new();
    for tick in 0..VIDEO_FRAMES {
        encoder
            .send_frame(&gray_frame(tick))
            .expect("send video frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv video") {
            packets.push(EncodedPacket::from_packet(pkt));
        }
    }
    encoder.send_eof().expect("video eof");
    while let Some(pkt) = encoder.receive_packet().expect("drain video") {
        packets.push(EncodedPacket::from_packet(pkt));
    }
    (packets, params, time_base)
}

/// A planar-float stereo frame of `samples` samples of low-level tone, stamped
/// at `pts` in the audio (`1/sample_rate`) time-base.
fn tone_frame(samples: usize, pts: i64) -> Audio {
    let mut frame = Audio::new(
        Sample::F32(SampleType::Planar),
        samples,
        ChannelLayout::STEREO,
    );
    frame.set_rate(SAMPLE_RATE);
    for ch in 0..2 {
        let plane = frame.plane_mut::<f32>(ch);
        for (i, sample) in plane.iter_mut().enumerate() {
            // A quiet, deterministic ramp — enough for AAC to emit real packets
            // without depending on a transcendental fn (and `as` is banned).
            let n = i32::try_from(i % 64).unwrap_or(0);
            *sample = f32::from(i16::try_from(n).unwrap_or(0)) / 4096.0;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

/// Encode roughly the video duration of `aac` audio once, returning the
/// kind-tagged audio packets plus its codec-params snapshot + time-base.
fn encode_audio() -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    let target = AudioEncodeTarget {
        codec_name: "aac".to_owned(),
        format: Sample::F32(SampleType::Planar),
        channel_layout: ChannelLayout::STEREO,
        sample_rate: SAMPLE_RATE,
        bit_rate: 128_000,
    };
    let mut encoder = AudioEncoder::new(&target).expect("open aac encoder");
    let time_base = encoder.time_base();
    let params = StreamCodecParameters::from_audio_encoder(&encoder);
    let frame_size = if encoder.frame_size() == 0 {
        1024
    } else {
        encoder.frame_size()
    };
    let frame_samples = usize::try_from(frame_size).unwrap_or(1024);
    // Cover ~the video span: VIDEO_FRAMES/FPS seconds of audio.
    let total_samples = (u64::try_from(VIDEO_FRAMES).unwrap_or(0) * u64::from(SAMPLE_RATE))
        / u64::try_from(FPS).unwrap_or(1);
    let blocks = (total_samples / u64::from(frame_size)) + 1;

    let mut packets = Vec::new();
    let mut pts: i64 = 0;
    for _ in 0..blocks {
        encoder
            .send_frame(&tone_frame(frame_samples, pts))
            .expect("send audio frame");
        while let Some(pkt) = encoder.receive_packet().expect("recv audio") {
            packets.push(EncodedPacket::from_audio_packet(pkt));
        }
        pts = pts.saturating_add(i64::from(frame_size));
    }
    encoder.send_eof().expect("audio eof");
    while let Some(pkt) = encoder.receive_packet().expect("drain audio") {
        packets.push(EncodedPacket::from_audio_packet(pkt));
    }
    (packets, params, time_base)
}

/// A [`PacketSource`] over a pre-built vector (drains front-to-back).
struct VecPackets {
    packets: std::vec::IntoIter<EncodedPacket>,
}

impl PacketSource for VecPackets {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.next())
    }
}

/// A packet's PTS on a common nanosecond timeline (exact i128 rational scale, no
/// float / no `as`), for interleaving the two streams the way the real pipeline
/// does per tick. An absent PTS sorts at the start.
fn pts_ns(pkt: &EncodedPacket, tb: Rational) -> i128 {
    let pts = i128::from(pkt.pts().unwrap_or(0));
    pts * i128::from(tb.num) * 1_000_000_000 / i128::from(tb.den)
}

/// Interleave the video + audio packets by their PTS on a common timeline (a
/// stable sort, so a video packet that ties an audio packet at the same instant
/// keeps the video first — the first segment must open on a video keyframe).
/// This mirrors the cli's per-tick A/V interleave; concatenating instead would
/// dump all audio into the final HLS segment.
fn interleave(
    video: Vec<EncodedPacket>,
    v_tb: Rational,
    audio: Vec<EncodedPacket>,
    a_tb: Rational,
) -> Vec<EncodedPacket> {
    let mut tagged: Vec<(i128, EncodedPacket)> = Vec::new();
    for pkt in video {
        tagged.push((pts_ns(&pkt, v_tb), pkt));
    }
    for pkt in audio {
        tagged.push((pts_ns(&pkt, a_tb), pkt));
    }
    tagged.sort_by_key(|(ns, _)| *ns);
    tagged.into_iter().map(|(_, pkt)| pkt).collect()
}

/// Count the DISTINCT stream indices of media type `kind` (`"v"` / `"a"`) in
/// `path`. MPEG-TS makes `ffprobe` list each stream twice (container scan +
/// post-analysis), so we dedupe by index rather than counting lines.
fn distinct_stream_indices(path: &Path, kind: &str) -> std::collections::BTreeSet<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-select_streams",
            kind,
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed on {}",
        path.display()
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<i64>().ok())
        .collect()
}

/// Assert `path` carries exactly one video stream and one audio stream.
fn assert_one_video_one_audio(path: &Path) {
    let video = distinct_stream_indices(path, "v");
    let audio = distinct_stream_indices(path, "a");
    assert_eq!(
        video.len(),
        1,
        "exactly one video stream in {}: {video:?}",
        path.display()
    );
    assert_eq!(
        audio.len(),
        1,
        "exactly one audio stream in {}: {audio:?}",
        path.display()
    );
}

#[test]
fn file_sink_muxes_a_video_plus_audio_container() {
    let (video, v_params, v_tb) = encode_video();
    let (audio, a_params, a_tb) = encode_audio();
    assert!(!video.is_empty(), "expected real video packets");
    assert!(!audio.is_empty(), "expected real audio packets");

    let combined = interleave(video, v_tb, audio, a_tb);
    let mut source = VecPackets {
        packets: combined.into_iter(),
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("program.ts");
    let sink = PacketMuxSink::file(&out);
    let outcome = sink
        .run_av(
            &mut source,
            MuxStream::new(&v_params, v_tb),
            Some(MuxStream::new(&a_params, a_tb)),
        )
        .expect("run_av file");
    match outcome {
        PacketMuxOutcome::Single(stats) => {
            assert!(stats.packets > 0, "muxed at least one packet");
        }
        PacketMuxOutcome::Segment(_) => panic!("file sink must yield Single"),
    }

    assert_one_video_one_audio(&out);
}

#[test]
fn segment_sink_muxes_video_plus_audio_per_segment() {
    let (video, v_params, v_tb) = encode_video();
    let (audio, a_params, a_tb) = encode_audio();

    let combined = interleave(video, v_tb, audio, a_tb);
    let mut source = VecPackets {
        packets: combined.into_iter(),
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let sink = PacketMuxSink::segment(dir.path(), "seg");
    let outcome = sink
        .run_av(
            &mut source,
            MuxStream::new(&v_params, v_tb),
            Some(MuxStream::new(&a_params, a_tb)),
        )
        .expect("run_av segment");
    let result = match outcome {
        PacketMuxOutcome::Segment(result) => result,
        PacketMuxOutcome::Single(_) => panic!("segment sink must yield Segment"),
    };
    assert!(!result.segments.is_empty(), "wrote at least one segment");
    for seg in &result.segments {
        assert_one_video_one_audio(seg);
    }
}
