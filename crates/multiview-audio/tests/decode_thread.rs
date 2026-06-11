// Per-source audio decode-thread tests (the off-by-default `ffmpeg` feature).
// Gated at the very top so the whole file is excluded under the default
// (pure-Rust) build.
#![cfg(feature = "ffmpeg")]
//! The audio decode loop is the peer of the video decode thread: it pulls
//! decoded+resampled blocks from an [`AudioFileDecoder`] and publishes them into
//! a bounded [`AudioStore`] the engine *samples* (never pacing, never blocking —
//! invariants #1/#10). Past end-of-stream the store reads silence, so a sampled
//! track is gap-free (ADR-R005 §4.1).
//!
//! Each test packages an in-memory sine to a lossless WAV via the `ffmpeg` CLI
//! (LGPL-clean — never x264/x265), exactly as `decode_meter.rs` does, so the
//! fixture is deterministic and needs no network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic-signal generation needs index<->float casts that are exact
// for the small ranges used here; test-only.
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
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use multiview_audio::store::{audio_decode_loop, AudioStore};
use multiview_audio::{AudioFormat, ChannelLayout};

const FS: u32 = 48_000;
const FREQ: f64 = 1000.0;

fn ffmpeg_cli_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

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

/// The decode loop drains a real fixture clip into the store, which then yields
/// 48 kHz stereo blocks of the decoded content followed by silence past EOF —
/// the store never gaps even though the decoder has ended.
#[test]
fn decode_loop_fills_store_then_silence_past_eof() {
    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decode-thread test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // 0.5 s of a clearly-non-silent tone (48_000 * 0.5 = 24_000 frames).
    let wav = encode_to_wav(dir.path(), &sine_interleaved(0.5, 0.5));

    // The decoder is opened *inside* the loop on the worker thread (the libav
    // context is not `Send`); the spawn carries only the `Send` path + layout,
    // exactly like the video `ingest_loop`.
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    let store = Arc::new(AudioStore::new(format, 96_000));
    let stop = Arc::new(AtomicBool::new(false));

    // Run the loop to completion on its own thread (the peer of the video decode
    // thread). It returns promptly at EOF without `stop` being raised.
    let loop_store = Arc::clone(&store);
    let loop_stop = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        audio_decode_loop(&wav, ChannelLayout::Stereo, &loop_store, &loop_stop);
    });
    handle.join().expect("decode loop thread panicked");

    // The whole 24_000-frame tone is buffered (bounded ring is 96k, so nothing
    // was evicted), then everything past it reads silence.
    let out = store.read(48_000);
    assert_eq!(out.frame_count(), 48_000);
    let s = out.interleaved();
    // The first ~24_000 frames carry the tone: at least some samples are
    // clearly non-zero (a 0.5-amplitude sine is not silence).
    let tone_energy: f64 = s[..48_000]
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum();
    assert!(
        tone_energy > 1.0,
        "decoded tone region is unexpectedly silent"
    );
    // The tail past EOF (frames ~24_000..48_000) is silence-filled, not a gap.
    let tail: f64 = s[48_000..].iter().map(|&v| f64::from(v).abs()).sum();
    assert_eq!(
        tail, 0.0,
        "frames past decoder EOF must be silence, not a gap"
    );
}

/// A "dead source" (decoder that yields nothing) must not wedge the loop: with
/// `stop` raised, the loop joins promptly (well under a wall second) — the
/// decode thread can never delay teardown, the audio analogue of the synth
/// `sleep_until` teardown guarantee.
#[test]
fn dead_source_loop_joins_promptly_on_stop() {
    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decode-thread test");
        return;
    }
    // A 1-frame WAV: the decoder reaches EOF almost immediately, so the loop
    // would exit on its own; we additionally raise `stop` to prove teardown is
    // bounded regardless of decoder state.
    let dir = tempfile::tempdir().unwrap();
    let wav = encode_to_wav(dir.path(), &sine_interleaved(0.1, 0.001));
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);

    let store = Arc::new(AudioStore::new(format, 4_096));
    let stop = Arc::new(AtomicBool::new(true)); // already requested to stop

    let loop_store = Arc::clone(&store);
    let loop_stop = Arc::clone(&stop);
    let start = std::time::Instant::now();
    let handle = std::thread::spawn(move || {
        audio_decode_loop(&wav, ChannelLayout::Stereo, &loop_store, &loop_stop);
    });
    handle.join().expect("decode loop thread panicked");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "decode loop did not tear down promptly: {elapsed:?}"
    );
}
