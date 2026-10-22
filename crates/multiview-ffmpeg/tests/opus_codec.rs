//! Opus packet decode + program-rendition encode tests (ADR-T014 §5,
//! ADR-0049, ADR-P006).
//!
//! * `OpusDecoder`: raw Opus packets (the RTP payload shape — no container, no
//!   extradata) -> 48 kHz stereo interleaved-`f32` blocks. Fixture packets are
//!   demuxed from a CLI-generated ogg/opus file (the established crate test
//!   pattern); ogg data packets ARE raw Opus packets, identical to RTP
//!   payloads (RFC 7587/3533).
//! * `OpusEncoder`: the single program Opus rendition — 48 kHz / 20 ms / stereo
//!   at ~96 kbps constrained-VBR ("CBR-ish"), `libopus` preferred with the
//!   native `opus` fallback, emitting audio-tagged [`EncodedPacket`]s.
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
use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::{
    Demuxer, OpusDecoder, OpusEncoder, StreamKind, OPUS_CHANNELS, OPUS_SAMPLE_RATE,
    PROGRAM_OPUS_BIT_RATE,
};
use tempfile::TempDir;

/// Generate one second of a 440 Hz stereo sine at amplitude 0.5, encoded with
/// the CLI's libopus into an ogg container.
fn generate_opus_ogg(dir: &Path) -> PathBuf {
    let out = dir.join("tone.ogg");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.5*sin(2*PI*440*t)|0.5*sin(2*PI*440*t):s=48000",
            "-t",
            "1",
            "-c:a",
            "libopus",
            "-b:a",
            "96k",
            "-f",
            "ogg",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate opus ogg");
    out
}

/// One demuxed Opus packet: its raw bytes and container PTS.
type OpusPacket = (Vec<u8>, Option<i64>);

/// Demux every Opus data packet `(bytes, pts)` from the fixture.
fn demux_opus_packets(path: &Path) -> (Vec<OpusPacket>, Rational) {
    let mut demux = Demuxer::open(path).expect("open ogg");
    let audio_index = demux
        .best_stream(MediaKind::Audio)
        .expect("ogg has an audio stream");
    let time_base = demux
        .streams()
        .into_iter()
        .find(|s| s.index == audio_index)
        .expect("stream params")
        .time_base;
    let mut packets = Vec::new();
    while let Some(pkt) = demux.read_packet_for(audio_index).expect("read") {
        let data = pkt.packet.data().expect("opus packet has data").to_vec();
        packets.push((data, pkt.packet.pts()));
    }
    assert!(
        packets.len() > 40,
        "1 s of 20 ms packets (got {})",
        packets.len()
    );
    (packets, time_base)
}

/// Root-mean-square of an interleaved block.
fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / u32::try_from(samples.len()).map(f64::from).unwrap()).sqrt()
}

#[test]
fn decodes_raw_opus_packets_to_stereo_48k_f32_blocks() {
    let dir = TempDir::new().unwrap();
    let (packets, time_base) = demux_opus_packets(&generate_opus_ogg(dir.path()));

    // RTP-shaped construction: no extradata, only the declared 48 kHz clock.
    let mut dec = OpusDecoder::new(time_base).expect("open opus decoder");
    let mut total_frames = 0_usize;
    let mut all_samples: Vec<f32> = Vec::new();
    let mut last_pts = i64::MIN;
    let drain = |dec: &mut OpusDecoder, total: &mut usize, all: &mut Vec<f32>, last: &mut i64| {
        while let Some(block) = dec.receive_block().expect("receive") {
            assert_eq!(block.rate, OPUS_SAMPLE_RATE, "blocks are 48 kHz");
            assert_eq!(block.channels, OPUS_CHANNELS, "blocks are stereo");
            assert_eq!(
                block.interleaved.len(),
                block.frame_count() * usize::from(block.channels),
                "interleaved length is frames x channels"
            );
            assert!(block.pts_nanos >= *last, "block PTS is non-decreasing");
            *last = block.pts_nanos;
            *total += block.frame_count();
            all.extend_from_slice(&block.interleaved);
        }
    };
    for (bytes, pts) in &packets {
        dec.push(bytes, *pts).expect("push opus packet");
        drain(&mut dec, &mut total_frames, &mut all_samples, &mut last_pts);
    }
    dec.send_eof().expect("eof");
    drain(&mut dec, &mut total_frames, &mut all_samples, &mut last_pts);

    // ~1 s of audio (pre-skip / final-packet padding tolerance).
    assert!(
        (45_000..=52_000).contains(&total_frames),
        "≈48000 frames decoded, got {total_frames}"
    );
    // A 0.5-amplitude sine has RMS ≈ 0.354; Opus at 96 kbps is near-transparent.
    // Measure the middle 80% to dodge pre-skip ramp-in and tail padding.
    let start = all_samples.len() / 10;
    let end = all_samples.len() - start;
    let mid_rms = rms(&all_samples[start..end]);
    assert!(
        (0.25..=0.45).contains(&mid_rms),
        "decoded energy is sane (RMS {mid_rms})"
    );
}

#[test]
fn opus_encoder_round_trips_pcm_with_sane_length_and_energy() {
    let mut enc = OpusEncoder::new(PROGRAM_OPUS_BIT_RATE).expect("open opus encoder");
    assert!(enc.frame_samples() > 0, "fixed 20 ms frame size");

    // One second of a 440 Hz stereo sine at amplitude 0.5, pushed in odd-sized
    // chunks to exercise the internal frame-size FIFO.
    let total_frames = usize::try_from(OPUS_SAMPLE_RATE).unwrap();
    let mut pcm = Vec::with_capacity(total_frames * 2);
    for i in 0..total_frames {
        // 48000 fits u16, so the f32 conversion is lossless and `as`-free.
        let t = f32::from(u16::try_from(i).unwrap()) / 48_000.0_f32;
        let s = 0.5_f32 * (2.0_f32 * std::f32::consts::PI * 440.0 * t).sin();
        pcm.push(s);
        pcm.push(s);
    }

    let mut packets = Vec::new();
    let drain = |enc: &mut OpusEncoder, packets: &mut Vec<_>| {
        while let Some(pkt) = enc.receive_packet().expect("recv") {
            assert_eq!(pkt.kind(), StreamKind::Audio, "audio-tagged packets");
            assert!(!pkt.is_empty(), "coded packets carry payload");
            packets.push(pkt);
        }
    };
    for chunk in pcm.chunks(2 * 1000) {
        enc.push_interleaved_f32(chunk).expect("push pcm");
        drain(&mut enc, &mut packets);
    }
    enc.finish().expect("finish");
    drain(&mut enc, &mut packets);

    // 1 s / 20 ms = 50 frames (the final partial frame is silence-padded).
    assert!(
        (48..=53).contains(&packets.len()),
        "≈50 packets for 1 s of 20 ms frames, got {}",
        packets.len()
    );
    // PTS is a sample counter (the audio analogue of the output tick).
    let pts: Vec<i64> = packets.iter().map(|p| p.pts().expect("pts")).collect();
    assert!(
        pts.windows(2).all(|w| w[1] > w[0]),
        "PTS strictly increases"
    );

    // Decode the coded packets back and check duration + energy.
    let mut dec = OpusDecoder::new(Rational::new(1, 48_000)).expect("open opus decoder");
    let mut decoded: Vec<f32> = Vec::new();
    let drain_dec = |dec: &mut OpusDecoder, decoded: &mut Vec<f32>| {
        while let Some(block) = dec.receive_block().expect("receive") {
            decoded.extend_from_slice(&block.interleaved);
        }
    };
    for pkt in &packets {
        let owned = pkt.to_owned_packet();
        dec.push(owned.data().expect("payload"), owned.pts())
            .expect("push coded packet");
        drain_dec(&mut dec, &mut decoded);
    }
    dec.send_eof().expect("eof");
    drain_dec(&mut dec, &mut decoded);

    let decoded_frames = decoded.len() / 2;
    assert!(
        (45_000..=52_000).contains(&decoded_frames),
        "≈1 s round-trips, got {decoded_frames} frames"
    );
    let start = decoded.len() / 10;
    let end = decoded.len() - start;
    let mid_rms = rms(&decoded[start..end]);
    assert!(
        (0.25..=0.45).contains(&mid_rms),
        "round-trip energy is sane (RMS {mid_rms})"
    );
}

#[test]
fn opus_encoder_prefers_libopus_with_native_fallback() {
    // The availability-probe contract (ADR-P006: "libopus, falling back to
    // libav's native opus"): whichever the linked FFmpeg provides, the
    // selection is honest and the encoder opens.
    let enc = OpusEncoder::new(PROGRAM_OPUS_BIT_RATE).expect("open opus encoder");
    if ffmpeg::encoder::find_by_name("libopus").is_some() {
        assert_eq!(enc.encoder_name(), "libopus", "libopus is preferred");
    } else {
        assert_eq!(
            enc.encoder_name(),
            "opus",
            "native opus is the fallback when libopus is absent"
        );
    }
}
