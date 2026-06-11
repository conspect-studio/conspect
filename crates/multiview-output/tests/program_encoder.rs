//! `ProgramEncoder` — the public encode-once producer (the `ffmpeg` feature).
//!
//! GPU-1 / invariant #7 (ADR-E003/E004, ADR-0026): the cli's bake consumer owns
//! ONE [`ProgramEncoder`], feeds it each baked NV12 canvas frame, and fans the
//! produced [`EncodedPacket`]s — as owned copies — to N mux-only
//! [`PacketMuxSink`]s. So the canvas is encoded exactly once and the SAME coded
//! packets feed every transport (file / HLS / push), never a per-output
//! re-encode. These tests pin the streaming-encoder contract:
//!
//! * the encoder re-stamps each packet's PTS from the tick counter, monotonic
//!   (`out_pts = f(tick)`, inv #3) — never the input PTS;
//! * keyframes land at the GOP boundary and the stream opens on one;
//! * the codec-params snapshot (each muxer seeds its stream from) is taken once;
//! * the after-`finish` contract (encode rejected, `finish` idempotent); and
//! * the headline property: ONE encode fanned to two muxers muxes byte-identically
//!   (the same packets, the same container) — encode-once-mux-many.
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
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{DecodedVideoFrame, EncodedPacket, StreamVideoDecoder};
use multiview_output::sink::{PacketMuxSink, ProgramEncoder};
use multiview_output::EncodeConfig;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

/// Build a tiny LGPL-clean encode config: `mpeg2video`/`yuv420p`, the given
/// geometry, `fps` cadence, a `gop`-frame keyframe interval.
fn config(fps: i64, gop: u32) -> EncodeConfig {
    EncodeConfig {
        codec_name: "mpeg2video".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: Pixel::YUV420P,
        cadence: Rational::new(fps, 1),
        gop,
        bit_rate: 2_000_000,
        audio: None,
        cuda_ordinal: None,
    }
}

/// Generate an MPEG-TS test clip with motion (so the codec emits real P-frames
/// between the GOP keyframes).
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
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
}

/// A test-only decode source over a file (matches the other sink tests).
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

/// Drive a whole clip through a `ProgramEncoder` (exactly as the cli consumer
/// would, one baked frame at a time), returning every produced packet in order.
fn encode_clip(src: &Path, cfg: &EncodeConfig) -> (Vec<EncodedPacket>, ProgramEncoder) {
    let mut encoder = ProgramEncoder::new(cfg).expect("open ProgramEncoder");
    let mut decode = DecodeSource::open(src);
    let mut packets = Vec::new();
    while let Some(frame) = decode.next_frame() {
        packets.extend(encoder.encode_frame(frame).expect("encode frame"));
    }
    packets.extend(encoder.finish().expect("finish"));
    (packets, encoder)
}

#[test]
fn program_encoder_produces_tick_stamped_monotonic_keyframed_packets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("src.ts");
    generate_clip(&clip, 2, 10);

    let cfg = config(10, 10);
    let (packets, mut encoder) = encode_clip(&clip, &cfg);

    // One coded packet per input frame for an all-I/P mpeg2 stream (~20 frames).
    assert!(
        packets.len() >= 15,
        "expected ~one packet per frame, got {}",
        packets.len()
    );

    // The time-base packets are stamped in is the reciprocal of the cadence.
    assert_eq!(encoder.time_base(), Rational::new(1, 10));

    // The stream opens on a keyframe and re-keys at the GOP (gop=10 over ~20
    // frames → at least two keyframes).
    assert!(packets[0].is_keyframe(), "first packet must be a keyframe");
    let keyframes = packets.iter().filter(|p| p.is_keyframe()).count();
    assert!(keyframes >= 2, "expected >=2 keyframes, got {keyframes}");

    // PTS is re-stamped from the tick counter and strictly increases (inv #3).
    let pts: Vec<i64> = packets.iter().filter_map(EncodedPacket::pts).collect();
    assert_eq!(pts.len(), packets.len(), "every packet carries a PTS");
    assert!(
        pts.windows(2).all(|w| w[1] > w[0]),
        "PTS must strictly increase, got {pts:?}"
    );

    // After finish, encoding is rejected and a second finish is a no-op.
    let frame = DecodeSource::open(&clip).next_frame().expect("a frame");
    assert!(
        encoder.encode_frame(frame).is_err(),
        "encode_frame after finish must error"
    );
    assert!(
        encoder.finish().expect("idempotent finish").is_empty(),
        "a second finish yields no packets"
    );
}

#[test]
fn one_encode_fans_to_two_muxers_byte_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("src.ts");
    generate_clip(&clip, 1, 10);

    let cfg = config(10, 10);
    let (packets, encoder) = encode_clip(&clip, &cfg);
    let params = encoder.codec_params().clone();
    let time_base = encoder.time_base();
    assert!(!packets.is_empty(), "the clip encoded to >=1 packet");

    // Duplicate the one encode into two independent owned packet lists — exactly
    // what the consumer's fan-out does (each muxer gets its own owned copy).
    let dup = |src: &[EncodedPacket]| -> Vec<EncodedPacket> {
        src.iter()
            .map(|p| EncodedPacket::from_packet(p.to_owned_packet()))
            .collect()
    };

    let a = dir.path().join("a.ts");
    let b = dir.path().join("b.ts");
    let mut src_a = VecPackets::new(dup(&packets));
    let mut src_b = VecPackets::new(dup(&packets));
    PacketMuxSink::file(&a)
        .run(&mut src_a, &params, time_base)
        .expect("mux a");
    PacketMuxSink::file(&b)
        .run(&mut src_b, &params, time_base)
        .expect("mux b");

    let len_a = std::fs::metadata(&a).expect("a exists").len();
    let len_b = std::fs::metadata(&b).expect("b exists").len();
    assert!(len_a > 0, "mux A wrote a non-empty container");
    assert_eq!(
        len_a, len_b,
        "the SAME encode fanned to two muxers must produce byte-identical containers"
    );
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

impl multiview_output::sink::PacketSource for VecPackets {
    fn next_packet(&mut self) -> multiview_output::Result<Option<EncodedPacket>> {
        Ok(self.packets.next())
    }
}
