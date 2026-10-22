//! OUTMETA mux-apply integration tests (the `ffmpeg` feature, ADR-0088 /
//! ADR-0089): a [`PacketMuxSink`] carrying `with_output_metadata(...)` actually
//! writes the requested container/stream metadata and the tag-path
//! display-rotation matrix into the muxed container — read back from the
//! bitstream with **ffprobe**, the C006-extended verification gate (never
//! assumed). One encode, fanned to a sink that tags; the tag/metadata add no
//! extra encode (invariant #7).
//!
//! Verified facts asserted here:
//! * MP4 container `title`/`comment` tags land (ffprobe `format_tags`).
//! * An MP4 video stream `DISPLAYMATRIX` side-data carries the requested
//!   rotation (ffprobe `side_data_list` → `rotation`).
//! * An MPEG-TS output carries the SDT `service_name`/`service_provider`
//!   (ffprobe `format_tags`).
//!
//! Licensing: encodes use `mpeg2video` (LGPL), never x264/x265 (GPL).
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
use multiview_core::layout::QuarterTurn;
use multiview_core::time::Rational;
use multiview_ffmpeg::{
    DecodedVideoFrame, EncodedPacket, ScaleSpec, Scaler, StreamCodecParameters, StreamVideoDecoder,
    VideoEncodeTarget, VideoEncoder,
};
use multiview_output::sink::{PacketMuxOutcome, PacketMuxSink, PacketSource};
use multiview_output::{display_matrix, MuxMetadata, Result};

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

/// Run ffprobe and return its raw stdout (JSON), failing the test on error.
fn ffprobe(args: &[&str]) -> String {
    let out = Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(args)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("ffprobe stdout is utf8")
}

// ---------------------------------------------------------------------------
// MP4: container title/comment + tag-path display matrix
// ---------------------------------------------------------------------------

#[test]
fn mp4_carries_container_tags_and_display_matrix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 1, 30);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    assert!(!packets.is_empty());

    let out = dir.path().join("program.mp4");

    let mut meta = MuxMetadata::new();
    meta.push_format("title", "Studio A Multiview").expect("ok");
    meta.push_format("comment", "Gallery confidence feed")
        .expect("ok");
    meta.push_stream(0, "language", "eng").expect("ok");

    // Tag-path orientation: declare a 90° clockwise display rotation.
    let matrix = display_matrix(QuarterTurn::Cw90);

    let sink = PacketMuxSink::file(&out).with_output_metadata(meta, Some(matrix));
    let mut source = VecPackets::new(packets);
    let outcome = sink
        .run(&mut source, &params, time_base)
        .expect("mp4 sink run");
    assert!(matches!(outcome, PacketMuxOutcome::Single(_)));

    // Verify the container tags landed (the C006-extended ffprobe gate).
    let fmt = ffprobe(&[
        "-show_entries",
        "format_tags=title:format_tags=comment",
        "-of",
        "default=noprint_wrappers=1",
        out.to_str().unwrap(),
    ]);
    assert!(
        fmt.contains("Studio A Multiview"),
        "MP4 container title must land — ffprobe said: {fmt}"
    );
    assert!(
        fmt.contains("Gallery confidence feed"),
        "MP4 container comment must land — ffprobe said: {fmt}"
    );

    // Verify the display-rotation matrix landed: ffprobe surfaces the rotation
    // as stream side data (Display Matrix) with the recovered angle.
    let sd = ffprobe(&[
        "-show_entries",
        "stream_side_data_list",
        "-select_streams",
        "v:0",
        "-of",
        "json",
        out.to_str().unwrap(),
    ]);
    let lower = sd.to_lowercase();
    assert!(
        lower.contains("displaymatrix") || lower.contains("display matrix"),
        "MP4 video stream must carry a DISPLAYMATRIX side data — ffprobe said: {sd}"
    );
    // ffprobe reports the clockwise-presentation rotation as -90 for a 90° CW
    // display matrix (libav's anticlockwise-angle convention); assert the
    // magnitude landed, not landscape (0).
    assert!(
        lower.contains("90"),
        "the rotation must be a quarter turn (90°) — ffprobe said: {sd}"
    );
    assert!(
        !lower.contains("rotation=0") && !lower.contains("\"rotation\": 0"),
        "the rotation must not be the identity — ffprobe said: {sd}"
    );
}

// ---------------------------------------------------------------------------
// MPEG-TS: SDT service_name / service_provider
// ---------------------------------------------------------------------------

#[test]
fn mpegts_carries_sdt_service_name_and_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 1, 30);

    let (packets, params, time_base) = encode_once(&src, 30, 30);
    assert!(!packets.is_empty());

    let out = dir.path().join("program.ts");

    // mpegtsenc reads `service_name` / `service_provider` from the format
    // metadata dict and writes them into the SDT.
    let mut meta = MuxMetadata::new();
    meta.push_format("service_name", "Studio A Multiview")
        .expect("ok");
    meta.push_format("service_provider", "Aperim Newsroom")
        .expect("ok");

    let sink = PacketMuxSink::file(&out).with_output_metadata(meta, None);
    let mut source = VecPackets::new(packets);
    sink.run(&mut source, &params, time_base)
        .expect("ts sink run");

    // mpegtsenc writes service_name/service_provider into the SDT; ffprobe
    // surfaces them as **program** tags (not format tags) on an MPEG-TS program.
    let progs = ffprobe(&["-show_programs", "-of", "json", out.to_str().unwrap()]);
    assert!(
        progs.contains("Studio A Multiview"),
        "MPEG-TS SDT service_name must land — ffprobe said: {progs}"
    );
    assert!(
        progs.contains("Aperim Newsroom"),
        "MPEG-TS SDT service_provider must land — ffprobe said: {progs}"
    );
}

// ---------------------------------------------------------------------------
// No metadata ⇒ unchanged container (the additive identity)
// ---------------------------------------------------------------------------

#[test]
fn no_metadata_leaves_container_untagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 1, 30);
    let (packets, params, time_base) = encode_once(&src, 30, 30);

    let out = dir.path().join("plain.mp4");
    // No with_output_metadata ⇒ the empty default; the container carries no
    // title we set.
    let sink = PacketMuxSink::file(&out);
    let mut source = VecPackets::new(packets);
    sink.run(&mut source, &params, time_base)
        .expect("plain sink run");

    let fmt = ffprobe(&[
        "-show_entries",
        "format_tags=title",
        "-of",
        "default=noprint_wrappers=1",
        out.to_str().unwrap(),
    ]);
    assert!(
        !fmt.contains("Studio A Multiview"),
        "an untagged sink must not invent a title — ffprobe said: {fmt}"
    );
}
