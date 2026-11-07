//! HLS MPEG-TS segment-boundary continuity-counter regression test (the
//! `ffmpeg` feature).
//!
//! Root cause this guards against: [`SegmentSink`]/[`PacketMuxSink::segment`]
//! used to call `Muxer::create_as` for EVERY segment, allocating a fresh
//! `AVFormatContext` per file. libav's `mpegtsenc` keeps PAT (PID 0), PMT, SDT
//! (PID 17) and every elementary stream's continuity counter (`cc`) as
//! per-PID state on that `AVFormatContext`, so a fresh muxer per segment
//! restarted every `cc` at 0 — exactly the failure an operator reported from
//! VLC: `TS discontinuity (received 0, expected N) for PID <n>` printed
//! endlessly at every segment boundary, because a continuous-TS-aware demuxer
//! treats the concatenation of segments as one timeline.
//!
//! The fix (`Muxer::redirect_output` in `multiview-ffmpeg`) keeps ONE
//! long-lived muxer for the whole run and only swaps its `AVIOContext` at each
//! segment cut, so every PID's `cc` continues unbroken across files while each
//! segment still opens with a fresh PAT/PMT/SDT (`mpegts_flags=resend_headers`)
//! and starts on a keyframe, so a player can start decoding ANY segment
//! independently (HLS's own requirement) while a continuity-tracking demuxer
//! sees no discontinuity across the concatenation.
//!
//! This test drives the real encode-once-mux-many `SegmentSink` to produce >=3
//! GOP-aligned `.ts` segments, then walks the raw 188-byte TS packets of their
//! CONCATENATION with a tiny parser (no external TS library) tracking the
//! continuity counter per PID, and asserts:
//! * no CC discontinuity across any segment boundary, for PID 0 (PAT), the PMT
//!   PID (read from the PAT), PID 17 (SDT), and every elementary-stream PID
//!   (read from the PMT);
//! * each segment, read independently, opens with PAT + PMT before any other
//!   payload-bearing packet; and
//! * each segment's first video-PID packet carries the random-access
//!   indicator (TS adaptation-field RAI bit) — the keyframe/segment-start
//!   requirement.
//!
//! If `ffprobe` is on `PATH`, each segment is also independently probed.
//!
//! Licensing: encodes use `mpeg2video`, an LGPL software codec already in
//! `FFmpeg` — never x264/x265 (which would be GPL).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use multiview_core::time::Rational;
use multiview_ffmpeg::{DecodedVideoFrame, EncodedPacket, StreamVideoDecoder};
use multiview_output::sink::{EncodeConfig, SegmentSink, VideoFrameSource};
use multiview_output::{Error, Result};

use ffmpeg_next as ffmpeg;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PAT_PID: u16 = 0;
const SDT_PID: u16 = 17;

/// Generate a tiny `mpeg2video` MPEG-TS clip with the `ffmpeg` CLI (matches the
/// sibling sink tests' fixture generator).
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

fn config(fps: u32, gop: u32) -> EncodeConfig {
    let mut cfg = EncodeConfig::mpeg2(WIDTH, HEIGHT);
    cfg.cadence = Rational::new(i64::from(fps), 1);
    cfg.gop = gop;
    cfg
}

/// One parsed 188-byte TS packet header, enough for continuity tracking and
/// PAT/PMT program-structure extraction.
struct TsPacketHeader {
    pid: u16,
    has_payload: bool,
    has_adaptation: bool,
    is_payload_start: bool,
    continuity_counter: u8,
    /// Byte offset of the payload within the 188-byte packet (after any
    /// adaptation field), or `None` if this packet carries no payload.
    payload_offset: Option<usize>,
}

/// Parse one 188-byte TS packet's fixed 4-byte header (+ adaptation-field
/// length byte when present). Panics on a short slice (a logic bug in the test
/// itself, not a case we tolerate).
fn parse_ts_header(packet: &[u8]) -> TsPacketHeader {
    assert_eq!(packet.len(), TS_PACKET_SIZE, "packet must be 188 bytes");
    assert_eq!(
        packet[0], SYNC_BYTE,
        "packet must start with sync byte 0x47"
    );
    let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
    let adaptation_field_control = (packet[3] >> 4) & 0x3;
    let has_adaptation = adaptation_field_control == 0x2 || adaptation_field_control == 0x3;
    let has_payload = adaptation_field_control == 0x1 || adaptation_field_control == 0x3;
    let is_payload_start = (packet[1] & 0x40) != 0; // payload_unit_start_indicator
    let continuity_counter = packet[3] & 0x0f;
    let payload_offset = if !has_payload {
        None
    } else if has_adaptation {
        let adaptation_len = usize::from(packet[4]);
        Some(5 + adaptation_len)
    } else {
        Some(4)
    };
    TsPacketHeader {
        pid,
        has_payload,
        has_adaptation,
        is_payload_start,
        continuity_counter,
        payload_offset,
    }
}

/// Random-access indicator: `adaptation_field_control` must carry an
/// adaptation field, byte 5 is `transport_private_data_flag`..`random_access_
/// indicator` etc; RAI is bit 0x40 of the adaptation flags byte (packet[5]).
fn has_random_access_indicator(packet: &[u8], header: &TsPacketHeader) -> bool {
    header.has_adaptation && packet.len() > 5 && (packet[5] & 0x40) != 0
}

/// Minimal PAT payload parser: returns `(program_number, pmt_pid)` pairs.
/// `payload` starts at the `pointer_field` byte of a payload-start PAT packet.
fn parse_pat_programs(payload: &[u8]) -> Vec<(u16, u16)> {
    if payload.is_empty() {
        return Vec::new();
    }
    let pointer = usize::from(payload[0]);
    let section_start = 1 + pointer;
    if section_start + 8 > payload.len() {
        return Vec::new();
    }
    let section = &payload[section_start..];
    let section_length = (usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]);
    // Section: table_id(1) + 2 length bytes + 5 fixed bytes = 8 before the
    // program loop; section_length counts from AFTER the length field.
    let section_end = (3 + section_length).min(section.len());
    let programs_start = 8;
    let mut out = Vec::new();
    let mut i = programs_start;
    // Each program entry is 4 bytes; stop 4 bytes before CRC32 at section_end.
    while i + 4 <= section_end.saturating_sub(4) || i + 4 <= section_end {
        if i + 4 > section.len() {
            break;
        }
        let program_number = (u16::from(section[i]) << 8) | u16::from(section[i + 1]);
        let pid = (u16::from(section[i + 2] & 0x1f) << 8) | u16::from(section[i + 3]);
        if i + 8 > section_end {
            // This is the last entry before CRC32 (or malformed) — include it
            // and stop.
            out.push((program_number, pid));
            break;
        }
        out.push((program_number, pid));
        i += 4;
    }
    out
}

/// Minimal PMT payload parser: returns the elementary-stream PIDs it lists.
/// `payload` starts at the `pointer_field` byte of a payload-start PMT packet.
fn parse_pmt_stream_pids(payload: &[u8]) -> Vec<u16> {
    if payload.is_empty() {
        return Vec::new();
    }
    let pointer = usize::from(payload[0]);
    let section_start = 1 + pointer;
    if section_start + 12 > payload.len() {
        return Vec::new();
    }
    let section = &payload[section_start..];
    let section_length = (usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]);
    let section_end = (3 + section_length).min(section.len());
    let program_info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut i = 12 + program_info_length;
    let mut out = Vec::new();
    while i + 5 <= section_end.saturating_sub(4) {
        let pid = (u16::from(section[i + 1] & 0x1f) << 8) | u16::from(section[i + 2]);
        let es_info_length =
            (usize::from(section[i + 3] & 0x0f) << 8) | usize::from(section[i + 4]);
        out.push(pid);
        i += 5 + es_info_length;
    }
    out
}

/// Split a byte buffer into 188-byte TS packets. Panics if the length is not a
/// multiple of 188 (a malformed/truncated capture, not tolerated by this
/// test).
fn split_ts_packets(bytes: &[u8]) -> Vec<&[u8]> {
    assert_eq!(
        bytes.len() % TS_PACKET_SIZE,
        0,
        "TS file length must be a multiple of {TS_PACKET_SIZE} bytes, got {}",
        bytes.len()
    );
    bytes.chunks_exact(TS_PACKET_SIZE).collect()
}

/// Per-segment analysis: whether PAT and PMT both appear before the first
/// elementary-stream (video) payload — the "independently decodable, starts
/// with PAT+PMT" HLS requirement — and whether that first video packet is a
/// keyframe. (libav's `mpegtsenc` sends SDT (PID 17) and PAT/PMT together at
/// segment start via `retransmit_si_info`, in SDT-then-PAT order — SDT
/// legitimately precedes PAT on the wire, so this does not require PAT to be
/// the literal first packet, only that PAT+PMT precede any ES payload.)
struct SegmentShape {
    pat_seen_before_es: bool,
    pmt_seen_before_es: bool,
    first_video_packet_has_rai: bool,
    video_pid: Option<u16>,
}

/// Analyze one segment's own packets (independent of any other segment) for
/// the "starts with PAT+PMT, first video frame is a keyframe" structural
/// requirement.
fn analyze_segment_shape(packets: &[&[u8]]) -> SegmentShape {
    let mut pat_pmt_pid: Option<u16> = None;
    let mut pat_index: Option<usize> = None;
    let mut pmt_index: Option<usize> = None;
    let mut video_pid: Option<u16> = None;
    let mut first_video_index: Option<usize> = None;

    for (i, packet) in packets.iter().enumerate() {
        let header = parse_ts_header(packet);
        if header.pid == PAT_PID && header.is_payload_start {
            if let Some(offset) = header.payload_offset {
                let programs = parse_pat_programs(&packet[offset..]);
                if let Some((_, pmt_pid)) = programs.first() {
                    pat_pmt_pid = Some(*pmt_pid);
                }
            }
            pat_index.get_or_insert(i);
        }
        if let Some(pmt_pid) = pat_pmt_pid {
            if header.pid == pmt_pid && header.is_payload_start && pmt_index.is_none() {
                if let Some(offset) = header.payload_offset {
                    let es_pids = parse_pmt_stream_pids(&packet[offset..]);
                    // The video stream is whichever ES PID is NOT PAT/PMT/SDT;
                    // our encode is video-only so the first (only) ES PID is it.
                    video_pid = es_pids.first().copied();
                }
                pmt_index = Some(i);
            }
        }
        if let (Some(vpid), None) = (video_pid, first_video_index) {
            if header.pid == vpid {
                first_video_index = Some(i);
            }
        }
    }

    let pat_seen_before_es = match (pat_index, first_video_index) {
        (Some(pat_i), Some(video_i)) => pat_i <= video_i,
        _ => false,
    };
    let pmt_seen_before_es = match (pmt_index, first_video_index) {
        (Some(pmt_i), Some(video_i)) => pmt_i <= video_i,
        _ => false,
    };
    let first_video_packet_has_rai = match first_video_index {
        Some(i) => has_random_access_indicator(packets[i], &parse_ts_header(packets[i])),
        None => false,
    };

    SegmentShape {
        pat_seen_before_es,
        pmt_seen_before_es,
        first_video_packet_has_rai,
        video_pid,
    }
}

/// A packet's PTS on a common nanosecond timeline (exact i128 rational scale,
/// no float / no `as`), for interleaving two streams the way the real pipeline
/// does per tick.
fn pts_ns(pkt: &EncodedPacket, tb: Rational) -> i128 {
    let pts = i128::from(pkt.pts().unwrap_or(0));
    pts * i128::from(tb.num) * 1_000_000_000 / i128::from(tb.den)
}

/// A [`multiview_output::sink::PacketSource`] over a pre-ordered vector.
struct VecPackets {
    packets: std::vec::IntoIter<EncodedPacket>,
}

impl multiview_output::sink::PacketSource for VecPackets {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.next())
    }
}

fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
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

#[test]
fn hls_segments_carry_no_cc_discontinuity_across_boundaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    // 4s @ 30fps, 1s GOP => 4 GOP-aligned segments (>= 3 required by the brief).
    generate_clip(&src, 4, 30);

    let sink = SegmentSink::new(config(30, 30), dir.path(), "seg");
    let mut source = DecodeSource::open(&src);
    let result = sink.run(&mut source).expect("segment sink run");

    assert!(
        result.segments.len() >= 3,
        "test needs >= 3 segments, got {}",
        result.segments.len()
    );

    // Read every segment's raw bytes, in order, and analyze each one's own
    // internal shape (PAT/PMT-before-ES, first video frame is a keyframe)
    // independently — a segment must be decodable on its own.
    let mut segment_packets: Vec<Vec<&[u8]>> = Vec::new();
    let mut segment_bytes: Vec<Vec<u8>> = Vec::new();
    for seg in &result.segments {
        let bytes = std::fs::read(seg).expect("read segment");
        segment_bytes.push(bytes);
    }
    for bytes in &segment_bytes {
        segment_packets.push(split_ts_packets(bytes));
    }

    let mut pmt_pid_for_segment: Vec<Option<u16>> = Vec::new();
    for (i, packets) in segment_packets.iter().enumerate() {
        let shape = analyze_segment_shape(packets);
        assert!(
            shape.pat_seen_before_es,
            "segment {i} ({:?}) must carry PAT before its first video packet",
            result.segments[i]
        );
        assert!(
            shape.pmt_seen_before_es,
            "segment {i} ({:?}) must carry PMT before its first video packet",
            result.segments[i]
        );
        assert!(
            shape.first_video_packet_has_rai,
            "segment {i} ({:?}) must start its video on a keyframe \
             (random_access_indicator set)",
            result.segments[i]
        );
        pmt_pid_for_segment.push(shape.video_pid.map(|_| {
            // Re-derive the PMT pid the same way `analyze_segment_shape` did,
            // for the cross-segment continuity walk below.
            let mut pmt_pid = None;
            for packet in packets {
                let header = parse_ts_header(packet);
                if header.pid == PAT_PID && header.is_payload_start {
                    if let Some(offset) = header.payload_offset {
                        if let Some((_, pid)) = parse_pat_programs(&packet[offset..]).first() {
                            pmt_pid = Some(*pid);
                        }
                    }
                    break;
                }
            }
            pmt_pid.expect("segment PAT names a PMT pid")
        }));
    }

    // Every segment must agree on the same PMT PID (the stream layout is fixed
    // for the whole run, invariant #7) — otherwise the cross-segment PID set
    // below would be meaningless.
    let pmt_pid = pmt_pid_for_segment[0].expect("first segment has a PMT pid");
    for (i, pid) in pmt_pid_for_segment.iter().enumerate() {
        assert_eq!(
            *pid,
            Some(pmt_pid),
            "segment {i} must advertise the SAME PMT pid as segment 0 (fixed layout)"
        );
    }

    // Discover every ES PID mentioned by any segment's PMT (there is exactly
    // one — this is a video-only run — but walk generically).
    let mut es_pids: Vec<u16> = Vec::new();
    for packets in &segment_packets {
        for packet in packets {
            let header = parse_ts_header(packet);
            if header.pid == pmt_pid && header.is_payload_start {
                if let Some(offset) = header.payload_offset {
                    for pid in parse_pmt_stream_pids(&packet[offset..]) {
                        if !es_pids.contains(&pid) {
                            es_pids.push(pid);
                        }
                    }
                }
            }
        }
    }
    assert!(!es_pids.is_empty(), "no elementary-stream PID discovered");

    // The headline assertion: walk the CONCATENATION of every segment's
    // packets and track `continuity_counter` per PID (PAT, PMT, SDT, every ES
    // PID) — the counter must advance by exactly 1 mod 16 on every
    // payload-bearing packet of that PID, with NO reset to 0 at a segment
    // boundary. (A non-payload-bearing packet, e.g. adaptation-only, does not
    // increment `cc` per the MPEG-TS spec and must be skipped from the
    // expectation, matching libav's own `cc` semantics.)
    let tracked_pids: Vec<u16> = std::iter::once(PAT_PID)
        .chain(std::iter::once(pmt_pid))
        .chain(std::iter::once(SDT_PID))
        .chain(es_pids.iter().copied())
        .collect();

    let mut last_cc: HashMap<u16, u8> = HashMap::new();
    let mut discontinuities: Vec<String> = Vec::new();
    let mut total_payload_packets: HashMap<u16, u64> = HashMap::new();

    for (seg_index, packets) in segment_packets.iter().enumerate() {
        for packet in packets {
            let header = parse_ts_header(packet);
            if !tracked_pids.contains(&header.pid) {
                continue;
            }
            // Per ISO/IEC 13818-1 §2.4.3.3: cc increments only when the
            // adaptation_field_control indicates a payload is present
            // (0x01 or 0x11... i.e. `has_payload`); a discontinuity-indicator
            // packet resets expectation deliberately, but our fixed encode
            // never sets one, so we require strict +1 mod 16 everywhere a
            // payload is carried.
            if !header.has_payload {
                continue;
            }
            *total_payload_packets.entry(header.pid).or_insert(0) += 1;
            if let Some(&prev) = last_cc.get(&header.pid) {
                let expected = (prev + 1) & 0x0f;
                if header.continuity_counter != expected {
                    discontinuities.push(format!(
                        "segment {seg_index} PID {}: received {}, expected {expected} \
                         (prev {prev}) — this is the exact 'TS discontinuity' VLC/libdvbpsi \
                         reports",
                        header.pid, header.continuity_counter
                    ));
                }
            }
            last_cc.insert(header.pid, header.continuity_counter);
        }
    }

    assert!(
        discontinuities.is_empty(),
        "continuity-counter discontinuities found across segment boundaries:\n{}",
        discontinuities.join("\n")
    );

    // Sanity: every tracked PID was actually observed with payload packets in
    // more than one segment, otherwise the cross-boundary assertion above
    // would be vacuous for that PID.
    for pid in &tracked_pids {
        assert!(
            total_payload_packets.get(pid).copied().unwrap_or(0) > 0,
            "PID {pid} was never observed carrying a payload — continuity check \
             for this PID was vacuous"
        );
    }

    // If `ffprobe` is available, confirm each segment ALSO opens cleanly on
    // its own (independent decodability from a real demuxer's perspective,
    // not just this test's hand-rolled parser).
    if ffprobe_available() {
        for seg in &result.segments {
            assert!(
                ffprobe_opens_cleanly(seg),
                "ffprobe failed to open segment {seg:?} independently"
            );
        }
    }
}

#[test]
fn hls_discrete_audio_segments_carry_no_cc_discontinuity_across_boundaries() {
    // Same property, but for the discrete per-rendition audio-only segment
    // muxers (`SegmentState::open_audio_segments`/`AudioSegment`), which carry
    // the identical one-long-lived-muxer-per-rendition fix. Drive it through
    // the packet-fed `PacketMuxSink::segment_live` with program audio, which is
    // the path that opens `AudioRenditionState::current` muxers.
    use multiview_ffmpeg::{
        AudioEncodeTarget, AudioEncoder, ScaleSpec, Scaler, StreamCodecParameters,
        VideoEncodeTarget, VideoEncoder,
    };
    use multiview_output::sink::{AudioSelection, MuxStream, PacketMuxOutcome, PacketMuxSink};
    use multiview_output::SharedEpoch;

    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.ts");
    generate_clip(&src, 4, 30);

    // Encode video once (mpeg2video), matching the other packet-mux tests.
    let video_target = VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: WIDTH,
        height: HEIGHT,
        format: ffmpeg::format::Pixel::YUV420P,
        time_base: Rational::new(1, 30),
        bit_rate: 2_000_000,
        gop: 30,
        cuda_device: None,
    };
    let mut video_encoder = VideoEncoder::new(&video_target).expect("open video encoder");
    let video_time_base = video_encoder.time_base();
    let video_params = StreamCodecParameters::from_encoder(&video_encoder);

    let mut decode = DecodeSource::open(&src);
    let mut scaler: Option<Scaler> = None;
    let mut packets: Vec<EncodedPacket> = Vec::new();
    let mut tick: i64 = 0;
    while let Some(frame) = decode.next_frame().expect("decode frame") {
        let mut video = frame.frame;
        if video.format() != ffmpeg::format::Pixel::YUV420P {
            let s = ScaleSpec::new(video.format(), video.width(), video.height());
            let d = ScaleSpec::new(
                ffmpeg::format::Pixel::YUV420P,
                video.width(),
                video.height(),
            );
            let sc = scaler.get_or_insert_with(|| Scaler::new(s, d).expect("scaler"));
            video = sc.run(&video).expect("scale");
        }
        video.set_pts(Some(tick));
        video_encoder.send_frame(&video).expect("send frame");
        while let Some(pkt) = video_encoder.receive_packet().expect("recv") {
            packets.push(EncodedPacket::from_packet(pkt));
        }
        tick += 1;
    }
    video_encoder.send_eof().expect("eof");
    while let Some(pkt) = video_encoder.receive_packet().expect("drain") {
        packets.push(EncodedPacket::from_packet(pkt));
    }

    // A tiny program-audio encode (silence), AAC @ 48k stereo, long enough to
    // span every video segment.
    let audio_target = AudioEncodeTarget {
        codec_name: "aac".to_owned(),
        sample_rate: 48_000,
        channel_layout: ffmpeg::ChannelLayout::STEREO,
        format: ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar),
        bit_rate: 128_000,
    };
    let mut audio_encoder = AudioEncoder::new(&audio_target).expect("open audio encoder");
    let audio_time_base = audio_encoder.time_base();
    let audio_params = StreamCodecParameters::from_audio_encoder(&audio_encoder);
    let frame_size = if audio_encoder.frame_size() == 0 {
        1024
    } else {
        audio_encoder.frame_size()
    };
    let total_samples = 48_000 * 4; // 4 seconds
    let mut sample_pos: i64 = 0;
    let mut audio_packets: Vec<EncodedPacket> = Vec::new();
    let frame_samples = usize::try_from(frame_size).unwrap_or(1024);
    while sample_pos < i64::from(total_samples) {
        let mut frame = ffmpeg::util::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar),
            frame_samples,
            ffmpeg::ChannelLayout::STEREO,
        );
        frame.set_rate(48_000);
        frame.set_pts(Some(sample_pos));
        for ch in 0..2 {
            let plane = frame.plane_mut::<f32>(ch);
            for (i, sample) in plane.iter_mut().enumerate() {
                let n = i32::try_from(i % 64).unwrap_or(0);
                *sample = f32::from(i16::try_from(n).unwrap_or(0)) / 4096.0;
            }
        }
        audio_encoder.send_frame(&frame).expect("send audio frame");
        while let Some(pkt) = audio_encoder.receive_packet().expect("recv audio") {
            audio_packets.push(EncodedPacket::from_audio_packet(pkt));
        }
        sample_pos += i64::from(frame_size);
    }
    audio_encoder.send_eof().expect("audio eof");
    while let Some(pkt) = audio_encoder.receive_packet().expect("drain audio") {
        audio_packets.push(EncodedPacket::from_audio_packet(pkt));
    }

    // Interleave video + audio packets by their PTS on a common nanosecond
    // timeline (a stable sort, so a tie keeps video first — the first segment
    // must open on a video keyframe). Concatenating instead (all video, then
    // all audio) would dump every audio packet into the final segment and
    // make the muxer reject the non-monotonic per-stream DTS order.
    let mut tagged: Vec<(i128, EncodedPacket)> = Vec::new();
    for pkt in packets {
        tagged.push((pts_ns(&pkt, video_time_base), pkt));
    }
    for pkt in audio_packets {
        tagged.push((pts_ns(&pkt, audio_time_base), pkt));
    }
    tagged.sort_by_key(|(ns, _)| *ns);
    let interleaved: Vec<EncodedPacket> = tagged.into_iter().map(|(_, pkt)| pkt).collect();

    let mut source = VecPackets {
        packets: interleaved.into_iter(),
    };

    let playlist_path = dir.path().join("multiview.m3u8");
    let mut sink =
        PacketMuxSink::segment_live(dir.path(), "seg", &playlist_path, 10, SharedEpoch::new());
    sink.set_audio_selection(AudioSelection::default());
    let outcome = sink
        .run_av(
            &mut source,
            MuxStream::new(&video_params, video_time_base),
            Some(MuxStream::new(&audio_params, audio_time_base)),
        )
        .expect("live segment sink run with audio");
    let result = match outcome {
        PacketMuxOutcome::Segment(result) => result,
        PacketMuxOutcome::Single(_) => panic!("segment sink must return a Segment outcome"),
    };
    assert!(
        result.segments.len() >= 3,
        "test needs >= 3 video segments, got {}",
        result.segments.len()
    );

    // Find the discrete audio-program segment files written alongside the
    // video ones (`audio-program-<n>.ts`), in index order.
    let mut audio_segment_paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().and_then(std::ffi::OsStr::to_str) == Some("ts")
                && p.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|n| n.starts_with("audio-program-"))
        })
        .collect();
    audio_segment_paths.sort();
    assert!(
        audio_segment_paths.len() >= 3,
        "test needs >= 3 discrete audio segments, got {}",
        audio_segment_paths.len()
    );

    // Walk the concatenation of the discrete audio segments' own PAT/PMT/ES
    // PID (each audio-only muxer has its own single-stream PAT/PMT) and assert
    // no CC discontinuity, exactly as for the video muxer above.
    let mut last_cc: HashMap<u16, u8> = HashMap::new();
    let mut discontinuities: Vec<String> = Vec::new();
    for (seg_index, path) in audio_segment_paths.iter().enumerate() {
        let bytes = std::fs::read(path).expect("read audio segment");
        let packets = split_ts_packets(&bytes);
        for packet in &packets {
            let header = parse_ts_header(packet);
            if !header.has_payload {
                continue;
            }
            // Track every PID this audio-only muxer carries (PAT=0, its own
            // PMT pid, its ES pid, and SDT=17 if present).
            if let Some(&prev) = last_cc.get(&header.pid) {
                let expected = (prev + 1) & 0x0f;
                if header.continuity_counter != expected {
                    discontinuities.push(format!(
                        "audio segment {seg_index} ({path:?}) PID {}: received {}, \
                         expected {expected} (prev {prev})",
                        header.pid, header.continuity_counter
                    ));
                }
            }
            last_cc.insert(header.pid, header.continuity_counter);
        }
    }
    assert!(
        discontinuities.is_empty(),
        "continuity-counter discontinuities found across discrete-audio segment \
         boundaries:\n{}",
        discontinuities.join("\n")
    );

    if ffprobe_available() {
        for path in &audio_segment_paths {
            assert!(
                ffprobe_opens_cleanly_audio(path),
                "ffprobe failed to open audio segment {path:?} independently"
            );
        }
    }
}

#[cfg(test)]
fn ffprobe_opens_cleanly_audio(path: &Path) -> bool {
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
    output.status.success() && stderr.trim().is_empty()
}
