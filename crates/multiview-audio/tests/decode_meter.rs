// Real audio decode + R128 metering tests (the off-by-default `ffmpeg`
// feature). Gated at the very top so the whole file is excluded under the
// default (pure-Rust) build — no contents are compiled or linted there.
#![cfg(feature = "ffmpeg")]
//! Real audio decode + R128 metering tests.
//!
//! These prove the libav decode/resample path (via multiview-ffmpeg's SAFE
//! wrappers) feeds the SAME pure-Rust [`LoudnessMeter`] / [`Mixer`] as the
//! in-memory path, and that a decoded sine of a known level measures the same
//! integrated loudness (within tolerance) as the identical sine metered
//! directly in memory.
//!
//! Each test is self-contained and deterministic: it synthesizes the *exact*
//! interleaved-`f32` sine in Rust, writes it to a tempdir as raw `f32le`, and
//! has the `ffmpeg` CLI encode that raw PCM to a `pcm_s16le` WAV (lossless,
//! LGPL-clean — never x264/x265). The reference meter runs on the in-memory
//! samples; the decode path runs on the WAV. They must agree within 16-bit
//! quantization tolerance, with no dependence on any source filter's
//! (implementation-defined) default amplitude.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic-signal generation needs index<->float and float<->sample
// casts that are exact for the small ranges used here; test-only.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]

use std::f64::consts::PI;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_audio::decode::{meter_file, AudioFileDecoder};
use multiview_audio::loudness::LoudnessMeter;
use multiview_audio::mixer::Mixer;
use multiview_audio::{AudioBlock, AudioFormat, ChannelLayout};

const FS: u32 = 48_000;
const FREQ: f64 = 1000.0;

/// Skip (do not fail) the suite if the `ffmpeg` CLI is not on PATH — it is only
/// needed to *package* the in-memory samples into a container the decode path
/// can open.
fn ffmpeg_cli_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Generate `seconds` of a `FREQ` Hz stereo sine of peak amplitude `amp` at
/// `FS` directly in memory as interleaved `f32` — the pure-Rust reference.
fn sine_interleaved(amp: f64, seconds: f64) -> Vec<f32> {
    let n = (f64::from(FS) * seconds).round() as usize;
    let mut out = Vec::with_capacity(n * 2);
    let w = 2.0 * PI * FREQ / f64::from(FS);
    for i in 0..n {
        let s = (amp * (w * i as f64).sin()) as f32;
        out.push(s);
        out.push(s);
    }
    out
}

/// Write interleaved-stereo `f32` samples to a raw `f32le` file, then have the
/// `ffmpeg` CLI losslessly package them into a `pcm_s16le` WAV. Returns the WAV
/// path. The samples in the WAV are exactly the in-memory ones, modulo 16-bit
/// quantization.
fn encode_to_wav(dir: &Path, samples: &[f32]) -> PathBuf {
    let raw = dir.join("sine.f32");
    let wav = dir.join("sine.wav");
    {
        let mut file = std::fs::File::create(&raw).expect("create raw f32 file");
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for &s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        file.write_all(&bytes).expect("write raw f32 samples");
    }
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-i",
        ])
        .arg(&raw)
        .args(["-c:a", "pcm_s16le"])
        .arg(&wav)
        .status()
        .expect("spawn ffmpeg to encode the WAV");
    assert!(status.success(), "ffmpeg failed to encode the WAV");
    wav
}

/// The decoded canonical format is 48 kHz / stereo (the resample target), and
/// the decoder actually yields audio.
#[test]
fn decoded_format_is_canonical_48k_stereo() {
    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decode test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = encode_to_wav(dir.path(), &sine_interleaved(0.5, 2.0));

    let mut decoder = AudioFileDecoder::open(&wav, ChannelLayout::Stereo).unwrap();
    assert_eq!(decoder.format().sample_rate(), AudioFormat::CANONICAL_RATE);
    assert_eq!(decoder.format().channel_layout(), ChannelLayout::Stereo);
    let first = decoder.next_block().unwrap();
    assert!(first.is_some(), "decoder produced no audio blocks");
}

/// THE CROSS-CHECK: a decoded sine must measure the SAME integrated loudness
/// (within tolerance) as the identical sine metered directly in memory through
/// the pure-Rust path. Any channel-order, scaling, or sample-format bug in the
/// decode→resample→interleave glue shifts the decoded reading away from the
/// reference. The two paths share only the raw float samples; the WAV round-trip
/// (f32 → s16 → libav decode → libswresample → f32) is exercised end to end.
#[test]
fn decoded_sine_matches_pure_rust_meter() {
    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decode test");
        return;
    }
    // -6 dBFS (amp 0.5): well clear of the gates, lots of 16-bit precision.
    let amp = 0.5;
    let seconds = 4.0;
    let samples = sine_interleaved(amp, seconds);

    // Pure-Rust reference: meter the in-memory samples directly.
    let mut reference = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
    reference.push_interleaved(&samples).unwrap();
    let l_ref = reference.integrated().unwrap();

    // libav decode path: package the SAME samples to WAV, decode, meter.
    let dir = tempfile::tempdir().unwrap();
    let wav = encode_to_wav(dir.path(), &samples);
    let meter = meter_file(&wav, ChannelLayout::Stereo).unwrap();
    let l_dec = meter
        .integrated()
        .expect("integrated loudness over a multi-second decoded tone");

    // 16-bit quantization + libswresample s16->f32 are sub-0.5 LU on a steady
    // -6 dBFS tone; the two BS.1770 measurements must coincide.
    approx::assert_abs_diff_eq!(l_dec, l_ref, epsilon = 0.5);
    // And it is genuinely loud (sanity: not silence-gated to None elsewhere).
    assert!(
        l_dec > -12.0 && l_dec < 0.0,
        "unexpected decoded loudness {l_dec}"
    );
}

/// A quieter (-23 dBFS) decoded tone — the EBU R128 alignment level — also
/// tracks the pure-Rust path, confirming the agreement is level-independent
/// (linearity through the whole decode chain), not a coincidence at one level.
#[test]
fn decoded_quiet_sine_tracks_reference_linearly() {
    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decode test");
        return;
    }
    let amp = 10f64.powf(-23.0 / 20.0); // -23 dBFS
    let seconds = 4.0;
    let samples = sine_interleaved(amp, seconds);

    let mut reference = LoudnessMeter::new(AudioFormat::new(FS, ChannelLayout::Stereo)).unwrap();
    reference.push_interleaved(&samples).unwrap();
    let l_ref = reference.integrated().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let wav = encode_to_wav(dir.path(), &samples);
    let meter = meter_file(&wav, ChannelLayout::Stereo).unwrap();
    let l_dec = meter.integrated().unwrap();

    // Quieter signal => coarser 16-bit quantization; allow a slightly wider band.
    approx::assert_abs_diff_eq!(l_dec, l_ref, epsilon = 0.7);
}

/// A decoded clip can be routed through the pure-Rust [`Mixer`] like any
/// in-memory block: the program bus of one routed decoded input (gain 1.0)
/// reproduces the decoded samples. This proves decode→mixer interop.
#[test]
fn decoded_blocks_feed_the_mixer() {
    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decode test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = encode_to_wav(dir.path(), &sine_interleaved(0.25, 1.0));

    let mut decoder = AudioFileDecoder::open(&wav, ChannelLayout::Stereo).unwrap();
    let format = decoder.format();
    let mut mixer = Mixer::new(format);
    let input = mixer.add_input("decoded");
    mixer.route_to_program(input, 1.0);

    let block: AudioBlock = decoder
        .next_block()
        .unwrap()
        .expect("at least one decoded block");
    let frames = block.frame_count();
    let original = block.interleaved().to_vec();
    mixer.submit(input, block).unwrap();

    let bus = mixer.mix_program().unwrap();
    assert_eq!(bus.frame_count(), frames);
    // Unit-gain single input => bus equals the input sample-for-sample.
    for (b, o) in bus.interleaved().iter().zip(original.iter()) {
        approx::assert_abs_diff_eq!(*b, *o, epsilon = 1e-6);
    }
}
