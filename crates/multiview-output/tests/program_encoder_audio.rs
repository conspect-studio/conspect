//! Program-audio encode path on `ProgramEncoder` (the `ffmpeg` feature) — AUD-4
//! slice 3b.
//!
//! `ProgramEncoder` owns the single video encode (invariant #7). With program
//! audio configured (`EncodeConfig.audio`) it also rebuffers the program bus's
//! variable-size sample blocks into the AAC encoder's fixed `frame_size` frames
//! and emits [`StreamKind::Audio`]-tagged packets, the audio peer of the video
//! `out_pts = f(tick)` (its PTS comes from a sample counter). These tests assert:
//!
//! * a configured encoder exposes the audio codec-params + time-base a mux sink
//!   registers its audio stream from, and turns planar f32 blocks into real
//!   audio-tagged packets; and
//! * a video-only encoder has no audio surface and `encode_audio` is a no-op.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::StreamKind;
use multiview_output::{AudioEncodeConfig, EncodeConfig, ProgramEncoder};

fn cfg_with_audio() -> EncodeConfig {
    let mut cfg = EncodeConfig::mpeg2(160, 120);
    cfg.audio = Some(AudioEncodeConfig::aac(48_000, 2, 128_000));
    cfg
}

#[test]
fn program_encoder_encodes_program_audio_into_tagged_packets() {
    let mut pe = ProgramEncoder::new(&cfg_with_audio()).expect("open program encoder");
    assert!(
        pe.audio_codec_params().is_some(),
        "audio stream params available for the mux sink to register"
    );
    assert_eq!(pe.audio_time_base(), Some(Rational::new(1, 48_000)));

    // ~1600 samples/tick (48 kHz @ 30 fps), a size that never lines up with the
    // 1024-sample AAC frame — the FIFO must rebuffer across ticks.
    let samples = 1600_usize;
    let left = vec![0.02_f32; samples];
    let right = vec![-0.02_f32; samples];

    let mut audio_packets = 0_usize;
    for _ in 0..20 {
        let pkts = pe
            .encode_audio(&[&left, &right], samples)
            .expect("encode program audio");
        for pkt in &pkts {
            assert_eq!(
                pkt.kind(),
                StreamKind::Audio,
                "audio packets are tagged Audio"
            );
        }
        audio_packets += pkts.len();
    }
    let tail = pe.finish().expect("finish flushes both encoders");
    let tail_audio = tail
        .iter()
        .filter(|p| p.kind() == StreamKind::Audio)
        .count();

    assert!(
        audio_packets + tail_audio > 0,
        "program audio produced AAC packets across the run"
    );
}

#[test]
fn program_encoder_encodes_interleaved_program_audio() {
    // The program bus hands the cli interleaved samples ([L, R, L, R, …]); the
    // encoder de-interleaves into the FIFO and produces audio-tagged packets.
    let mut pe = ProgramEncoder::new(&cfg_with_audio()).expect("open");
    let frames = 1600_usize;
    let interleaved = vec![0.015_f32; frames * 2];

    let mut audio_packets = 0_usize;
    for _ in 0..20 {
        let pkts = pe
            .encode_audio_interleaved(&interleaved, frames)
            .expect("encode interleaved audio");
        for pkt in &pkts {
            assert_eq!(pkt.kind(), StreamKind::Audio);
        }
        audio_packets += pkts.len();
    }
    let tail = pe.finish().expect("finish");
    let tail_audio = tail
        .iter()
        .filter(|p| p.kind() == StreamKind::Audio)
        .count();
    assert!(
        audio_packets + tail_audio > 0,
        "interleaved program audio produced AAC packets"
    );
}

#[test]
fn video_only_program_encoder_has_no_audio_surface() {
    let mut pe = ProgramEncoder::new(&EncodeConfig::mpeg2(160, 120)).expect("open");
    assert!(pe.audio_codec_params().is_none());
    assert_eq!(pe.audio_time_base(), None);
    // With no audio configured, encode_audio is a harmless no-op (the consumer
    // can call it unconditionally without first checking).
    let pkts = pe.encode_audio(&[], 0).expect("no-op");
    assert!(pkts.is_empty(), "video-only encoder emits no audio packets");
}
