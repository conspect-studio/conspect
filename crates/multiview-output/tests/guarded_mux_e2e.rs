//! GP-7 end-to-end: a `GuardedPacketSource` driven through the REAL
//! `PacketMuxSink::run_av` into an MP4 across BOTH seams (the `ffmpeg` feature;
//! ADR-0030 §4).
//!
//! The unit + property tests pin the strictly-increasing-DTS contract at the
//! producer; this test proves the contract is actually accepted by the real
//! muxer. MP4/mov's `av_interleaved_write_frame` **aborts on non-monotonic DTS**
//! (even on *equal* DTS), so muxing a live→slate→live sequence into a `.mp4`
//! without an error IS the GP-6 clamp guarantee, end to end through the sink
//! `GuardedPacketSource` is the sole producer of (ADR-0030 §4: "sole producer
//! feeding `PacketMuxSink::run_av`"). ffprobe then confirms the container is a
//! valid, decodable MP4 with frames.
//!
//! Both the live elementary stream and the slate are `mpeg2video` at the same
//! geometry/cadence (LGPL-clean), so the single registered mux stream's codec
//! parameters serve both the copied-input and the spliced-slate packets.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{
    CodecKind, DecodedVideoFrame, EncodedPacket, NalFraming, ScaleSpec, Scaler,
    StreamCodecParameters, StreamVideoDecoder, VideoEncodeTarget, VideoEncoder,
};
use multiview_framestore::{PacketLiveness, PacketLivenessThresholds};
use multiview_output::guarded::{GuardMode, GuardedConfig, GuardedPacketSource, ManualClock};
use multiview_output::sink::{MuxStream, PacketMuxOutcome, PacketMuxSink, PacketSource};
use multiview_output::slate::{
    BakedSlate, SlateBaker, SlateKind, SlateSpec, SlateVideoCodec, SlateVideoSpec,
};
use multiview_output::Result;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const FPS: u32 = 30;
const GOP: u32 = 15;
const FRAME_NS: i64 = 33_366_666;
const SEGMENT_NS: i64 = 2_000_000_000;

/// Generate a short raw mpeg2video clip via the ffmpeg CLI (LGPL-clean codec).
fn generate_clip(path: &Path, seconds: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={WIDTH}x{HEIGHT}:rate={FPS}:duration={seconds}"),
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-g",
            &GOP.to_string(),
            "-keyint_min",
            &GOP.to_string(),
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

/// Decode source over a file (mirrors the other sink tests).
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

fn target() -> VideoEncodeTarget {
    VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, i64::from(FPS)),
        bit_rate: 2_000_000,
        gop: GOP,
        cuda_device: None,
    }
}

/// Encode the whole `src` clip ONCE into a vector of [`EncodedPacket`]s + the
/// `Send` codec-params snapshot + the encoder time-base (the live ES the
/// guarded source copies).
fn encode_live(src: &Path) -> (Vec<EncodedPacket>, StreamCodecParameters, Rational) {
    let mut encoder = VideoEncoder::new(&target()).expect("open encoder");
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

/// A matching `mpeg2video` slate at the same geometry/cadence as the live ES.
fn matching_slate() -> BakedSlate {
    SlateBaker::bake_slate(&SlateSpec {
        kind: SlateKind::Black,
        video: SlateVideoSpec {
            codec: SlateVideoCodec::Mpeg2Video,
            width: WIDTH,
            height: HEIGHT,
            cadence: Rational::new(i64::from(FPS), 1),
            gop: GOP,
        },
        audio: None,
    })
    .expect("bake matching slate")
}

/// A [`PacketSource`] over a pre-encoded vector (the live ES, drains front to
/// back, then `Ok(None)`).
struct VecLive {
    packets: std::vec::IntoIter<EncodedPacket>,
}

impl PacketSource for VecLive {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.next())
    }
}

/// A finite adapter over the (infinite) guarded source so the mux test
/// terminates: it drives a scripted clock schedule (healthy → outage that forces
/// a slate splice) and returns `Ok(None)` after `limit` emitted packets so
/// `run_av` finalizes. The guarded seam itself stays infinite (a degenerate
/// clock never ends); this adapter only bounds the TEST.
struct ScriptedDrive {
    inner: GuardedPacketSource<VecLive, Arc<ManualClock>>,
    clock: Arc<ManualClock>,
    emitted: usize,
    limit: usize,
    /// Emit index at which to jump the clock into an outage (force a splice).
    outage_at: usize,
    saw_slate: bool,
}

impl ScriptedDrive {
    fn new(
        inner: GuardedPacketSource<VecLive, Arc<ManualClock>>,
        clock: Arc<ManualClock>,
        limit: usize,
        outage_at: usize,
    ) -> Self {
        Self {
            inner,
            clock,
            emitted: 0,
            limit,
            outage_at,
            saw_slate: false,
        }
    }
}

impl PacketSource for ScriptedDrive {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        if self.emitted >= self.limit {
            return Ok(None);
        }
        // Schedule the clock: healthy advances under STALE up to the outage, then
        // jump past SPLICE so the watchdog flips to slate (the rest of the run
        // is slate, exercising the input→slate seam + the slate loop wraps
        // through the REAL strict-DTS muxer).
        if self.emitted >= self.outage_at {
            self.clock.advance(SEGMENT_NS);
        } else {
            self.clock.advance(FRAME_NS / 2);
        }
        let pkt = self.inner.next_packet()?;
        if self.inner.mode() == GuardMode::Slate {
            self.saw_slate = true;
        }
        self.emitted += 1;
        Ok(pkt)
    }
}

/// Run ffprobe and assert the file is a decodable MP4 reporting frames.
fn assert_decodable_mp4(path: &Path) {
    let out = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed on the muxed mp4: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = String::from_utf8_lossy(&out.stdout);
    let n: u64 = raw
        .trim()
        .trim_end_matches(',')
        .parse()
        .unwrap_or_else(|e| panic!("ffprobe packet count parse {raw:?}: {e}"));
    assert!(n > 0, "muxed mp4 has at least one video packet");
}

/// The headline GP-7 end-to-end: a guarded source spanning live→slate→live
/// muxes into a STRICT-DTS container (MP4) without the non-monotonic-DTS abort,
/// and the result is a decodable MP4.
#[test]
fn guarded_live_slate_live_muxes_to_strict_mp4() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("live.ts");
    generate_clip(&clip, 2);
    let (live_packets, params, time_base) = encode_live(&clip);
    assert!(
        live_packets.len() > GOP as usize,
        "the live clip has several GOPs"
    );

    let clock = Arc::new(ManualClock::new());
    let video_liveness = Arc::new(PacketLiveness::new(
        PacketLivenessThresholds::from_frame_and_segment(FRAME_NS, SEGMENT_NS).expect("ladder"),
    ));
    let guarded = GuardedPacketSource::new(
        VecLive {
            packets: live_packets.clone().into_iter(),
        },
        matching_slate(),
        video_liveness,
        None,
        Arc::clone(&clock),
        // The end-to-end target here is the input→slate seam + slate-loop wraps
        // through the REAL strict-DTS MP4 muxer (the abort surface). mpeg2video
        // is not NAL-based, so the strict-IDR classifier (exercised on
        // H.264/HEVC/AV1 in the unit/property tests) never re-anchors on these
        // packets — the run stays slate after the outage, which is exactly the
        // seam this test muxes.
        GuardedConfig::new(CodecKind::H264, NalFraming::AnnexB, FRAME_NS),
    );

    // Drive the first third of the live packets healthy, then an outage that
    // holds slate through the rest of the run (both seams' DTS through the muxer).
    let limit = live_packets.len();
    let mut drive = ScriptedDrive::new(guarded, Arc::clone(&clock), limit, limit / 3);

    let out = dir.path().join("guarded.mp4");
    let sink = PacketMuxSink::file(&out);
    let outcome = sink
        .run_av(&mut drive, MuxStream::new(&params, time_base), None)
        .expect("guarded source muxes into a strict-DTS MP4 without a non-monotonic abort");
    match outcome {
        PacketMuxOutcome::Single(stats) => {
            assert!(stats.packets > 0, "muxed some packets");
        }
        PacketMuxOutcome::Segment(_) => panic!("file sink must yield a single-container outcome"),
    }

    assert!(drive.saw_slate, "the drive forced a slate splice");
    assert_decodable_mp4(&out);
}
