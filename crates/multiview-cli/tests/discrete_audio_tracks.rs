//! End-to-end proof of discrete audio tracks on the HLS output (#152, feature
//! `ffmpeg`): an `[audio]` route with a `target_track` is encoded once and muxed
//! into every TS segment as an extra audio PID, stamped from the same sample
//! clock as the program audio. Every assertion is against the on-disk segments.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
// reason: test-only float maths over decoded samples and ffprobe's seconds.

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;

const TICKS: u64 = 100; // 4 s @ 25 fps.
const TRACK_HZ: u32 = 440;
/// One AAC frame at 48 kHz, in seconds.
const AAC_FRAME_S: f64 = 1024.0 / 48_000.0;

fn build_tone_clip(path: &Path, seconds: u32) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .arg("-i")
        .arg(format!("testsrc=size=320x240:rate=25:duration={seconds}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={TRACK_HZ}:sample_rate=44100:duration={seconds}"
        ))
        .args(["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg fixture failed");
}

/// `in_a` (bars, 1 kHz) is the program; `in_b` (a 44.1 kHz 440 Hz clip, in no
/// cell, muted on the program bus) is decoded and resampled into the 48 kHz
/// clean discrete track, unaffected by its program-bus gain and mute.
fn config_text(clip: &Path, playlist: &Path, output_audio: &str) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 640
height = 360
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "bars"

[[sources]]
id = "in_b"
kind = "file"
path = "{clip}"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
{output_audio}

[audio]
sample_rate_hz = 48000

[[audio.routes]]
input_id = "in_a"
channels = {{ kind = "stereo" }}
include_in_program_bus = true

[[audio.routes]]
input_id = "in_b"
channels = {{ kind = "stereo" }}
target_track = "au-abc"
language = "eng"
title = "AU ABC"
include_in_program_bus = true
gain_db = -18.0
mute = true
"##,
        clip = clip.display(),
        playlist = playlist.display(),
    )
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read out dir")
        .map(|e| e.expect("dirent").path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "ts")
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("seg"))
        })
        .collect();
    out.sort_by_key(|p| {
        p.file_stem()
            .and_then(|s| {
                s.to_string_lossy()
                    .trim_start_matches("seg")
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0)
    });
    out
}

/// `(codec_type, start_time, language)` per stream, by stream index.
fn probe_streams(path: &Path) -> Vec<(String, f64, String)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=index,codec_type,start_time:stream_tags=language",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
    doc["streams"]
        .as_array()
        .expect("streams")
        .iter()
        .map(|s| {
            (
                s["codec_type"].as_str().unwrap_or_default().to_owned(),
                s["start_time"]
                    .as_str()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(f64::NAN),
                s["tags"]["language"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

/// Goertzel power of `hz` in `samples` at 48 kHz.
fn tone_power(samples: &[f32], hz: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * hz / 48_000.0;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// Decode the complete stream; never discard its startup or segment boundaries.
fn decode_audio(path: &Path, stream: &str) -> Vec<f32> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map", stream, "-f", "f32le", "-ac", "1", "-ar", "48000", "pipe:1",
        ])
        .output()
        .expect("spawn ffmpeg decode");
    assert!(out.status.success(), "decode of {stream} failed");
    assert_eq!(out.stdout.len() % 4, 0, "incomplete PCM sample");
    out.stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Complete-signal coverage plus local spectral identity. A one-window tone
/// cannot satisfy the duration, voiced-fraction, and maximum-gap checks.
#[allow(clippy::print_stdout)] // Expose decoded PCM metrics for the targeted release proof.
fn assert_audible_tone(
    samples: &[f32],
    label: &str,
    expected_hz: f64,
    other_hz: f64,
    expected_duration: f64,
) {
    const WINDOW: usize = 480; // 10 ms; also bounds the gap measurement.
    let duration = samples.len() as f64 / 48_000.0;
    let mut voiced = 0usize;
    let mut gap = 0usize;
    let mut max_gap = 0usize;
    let mut expected_power = 0.0;
    let mut other_power = 0.0;
    let mut crossings = 0usize;
    for window in samples.chunks(WINDOW) {
        let energy = window
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / window.len() as f64;
        if energy < 1e-6 {
            gap += window.len();
            max_gap = max_gap.max(gap);
            continue;
        }
        voiced += window.len();
        gap = 0;
        expected_power += tone_power(window, expected_hz) / (window.len() * window.len()) as f64;
        other_power += tone_power(window, other_hz) / (window.len() * window.len()) as f64;
        let mut prev = 0i32;
        for &sample in window {
            let sign = if sample > 1e-4 {
                1
            } else if sample < -1e-4 {
                -1
            } else {
                prev
            };
            if sign != 0 && prev != 0 && sign != prev {
                crossings += 1;
            }
            if sign != 0 {
                prev = sign;
            }
        }
    }
    let fraction = voiced as f64 / samples.len().max(1) as f64;
    let max_gap_s = max_gap as f64 / 48_000.0;
    let observed_hz = crossings as f64 / (2.0 * voiced.max(1) as f64 / 48_000.0);
    println!("{label}: decoded_pcm_duration={duration:.3}s voiced_fraction={fraction:.4} max_gap={max_gap_s:.3}s frequency={observed_hz:.1}Hz");
    assert!(
        (expected_duration - 0.05..=expected_duration + 0.15).contains(&duration),
        "{label}: wrong decoded duration {duration:.3}s"
    );
    // AAC's one-frame encoder priming and tail permit at most two frames total.
    // The six-second source remains continuous throughout the four-second run.
    assert!(
        fraction > 0.98,
        "{label}: insufficient voiced coverage {fraction:.4}"
    );
    assert!(
        max_gap_s <= 2.0 * AAC_FRAME_S,
        "{label}: silence gap {max_gap_s:.3}s"
    );
    assert!(
        (observed_hz - expected_hz).abs() < 50.0,
        "{label}: expected {expected_hz}Hz, got {observed_hz}Hz"
    );
    assert!(
        expected_power > 10.0 * other_power,
        "{label}: wrong tone ({expected_power:.3e} vs {other_power:.3e})"
    );
}

#[test]
fn audio_detector_rejects_short_or_wrong_tones_and_accepts_phase_changes() {
    let mut good: Vec<f32> = (0..192_000)
        .map(|i| (0.1 * (std::f64::consts::TAU * 440.0 * f64::from(i) / 48_000.0).sin()) as f32)
        .collect();
    for (index, sample) in good.iter_mut().enumerate().skip(96_000) {
        *sample =
            (0.1 * (std::f64::consts::TAU * 440.0 * index as f64 / 48_000.0 + 1.0).sin()) as f32;
    }
    assert_audible_tone(&good, "phase-varied positive", 440.0, 1000.0, 4.0);
    let mut mostly_silent = good.clone();
    mostly_silent[4_800..].fill(0.0);
    assert!(std::panic::catch_unwind(|| assert_audible_tone(
        &mostly_silent,
        "mostly silent negative",
        440.0,
        1000.0,
        4.0
    ))
    .is_err());
    assert!(std::panic::catch_unwind(|| assert_audible_tone(
        &good,
        "wrong tone negative",
        1000.0,
        440.0,
        4.0
    ))
    .is_err());
}

async fn run(output_audio: &str) -> (tempfile::TempDir, PathBuf) {
    // These are five full realtime encoders and decoders. Serialise the source
    // runs so parallel test workers cannot starve each other's finite audio.
    static RUN_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let _permit = RUN_GATE.acquire().await.expect("run gate stays open");
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("tone.ts");
    build_tone_clip(&clip, 6);
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let config = MultiviewConfig::load_from_toml(&config_text(&clip, &playlist, output_audio))
        .expect("parse config");
    config.validate().expect("config validates");
    let mut pipeline = Pipeline::build(&config).expect("build pipeline");
    pipeline.enable_program_audio();
    let report = pipeline.run_for(TICKS).await.expect("bounded run");
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(!report.faltered, "tracks must not falter the output");
    (dir, out_dir)
}

/// The acceptance proof: video + program + one discrete track in EVERY segment,
/// with audio start PTS within one AAC frame, and the track carrying the discrete
/// input (440 Hz) rather than the program (1 kHz).
async fn assert_two_audio_streams(output_audio: &str) {
    let (_guard, out_dir) = Box::pin(run(output_audio)).await;
    let segs = segments(&out_dir);
    assert!(!segs.is_empty(), "no segments written");
    let mut joined = Vec::new();
    let mut segment_samples: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    for seg in &segs {
        let streams = probe_streams(seg);
        let kinds: Vec<&str> = streams.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            kinds,
            ["video", "audio", "audio"],
            "stream layout of {}",
            seg.display()
        );
        assert_eq!(streams[2].2, "eng", "track language in {}", seg.display());
        let skew = (streams[1].1 - streams[2].1).abs();
        assert!(
            skew <= AAC_FRAME_S,
            "program/track start PTS skew {skew} s in {}",
            seg.display()
        );
        for (index, label, hz, other) in [
            (0, "program", 1000.0, 440.0),
            (1, "discrete", 440.0, 1000.0),
        ] {
            let pcm = decode_audio(seg, &format!("0:a:{index}"));
            let duration = pcm.len() as f64 / 48_000.0;
            assert!(
                duration >= 0.94,
                "{label}: short segment {} duration={duration:.3}s",
                seg.display()
            );
            // The final segment can contain fewer than 25 video frames; AAC
            // sample count, rather than TS byte count, proves occupancy.
            let voiced = pcm
                .chunks(480)
                .filter(|window| {
                    window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32 >= 1e-6
                })
                .count();
            let occupancy = voiced as f64 / pcm.len().div_ceil(480) as f64;
            assert!(
                occupancy > 0.95,
                "{label}: segment {} voiced fraction={occupancy:.4}",
                seg.display()
            );
            let expected = tone_power(&pcm, hz);
            let wrong = tone_power(&pcm, other);
            assert!(
                expected > 10.0 * wrong,
                "{label}: wrong tone in {}",
                seg.display()
            );
            segment_samples[index].push(pcm.len());
        }
        joined.extend(std::fs::read(seg).expect("read segment"));
    }
    let all = out_dir.join("joined.bin");
    std::fs::write(&all, joined).expect("write joined");
    for (index, label, hz, other) in [
        (0, "program", 1000.0, 440.0),
        (1, "discrete", 440.0, 1000.0),
    ] {
        let pcm = decode_audio(&all, &format!("0:a:{index}"));
        // Inspect the join in the continuous decoder, not isolated AAC
        // decoders whose priming creates a false sample-to-sample jump.
        let mut offset = 0usize;
        for &segment_len in segment_samples[index].iter().take(segs.len() - 1) {
            offset += segment_len;
            let before = *pcm.get(offset - 1).expect("joined segment end");
            let after = *pcm.get(offset).expect("joined segment start");
            let jump = (before - after).abs();
            assert!(
                jump < 0.05,
                "{label}: joined PCM phase jump {jump:.4} at sample {offset}"
            );
        }
        assert_audible_tone(&pcm, label, hz, other, 4.0);
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Full manifest traversal and real decode belong in one end-to-end assertion.
async fn master_resolves_playable_distinct_audio_only_renditions() {
    let (_guard, dir) = Box::pin(run("[outputs.audio]\nmode = \"auto\"")).await;
    let master = std::fs::read_to_string(dir.join("master.m3u8")).expect("master");
    assert!(master.contains("NAME=\"Program\",DEFAULT=YES"), "{master}");
    assert!(master.contains("NAME=\"AU ABC\",DEFAULT=NO"), "{master}");
    assert!(master.contains("LANGUAGE=\"eng\""), "{master}");
    assert!(master.contains("AUDIO=\"audio\""), "{master}");
    let video_uri = master
        .lines()
        .find(|line| {
            line.starts_with("video-g")
                && Path::new(line)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
        })
        .expect("generation-scoped video URI");
    let playable_master = probe_streams(&dir.join("master.m3u8"));
    assert!(
        playable_master.iter().any(|stream| stream.0 == "video"),
        "master lacks video"
    );
    assert!(
        playable_master.iter().any(|stream| stream.0 == "audio"),
        "master lacks default audio"
    );
    let video = std::fs::read_to_string(dir.join(video_uri)).expect("video media playlist");
    for uri in video.lines().filter(|line| {
        Path::new(line)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
    }) {
        assert_eq!(
            probe_streams(&dir.join(uri))
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>(),
            ["video"]
        );
    }
    for (prefix, frequency) in [("audio-program-g", 1000.0), ("audio-track0-g", 440.0)] {
        let playlist = master
            .lines()
            .find(|line| {
                line.starts_with("#EXT-X-MEDIA:") && line.contains(&format!("URI=\"{prefix}"))
            })
            .and_then(|line| line.split("URI=\"").nth(1))
            .and_then(|uri| uri.split('"').next())
            .expect("generation-scoped audio URI");
        let media = std::fs::read_to_string(dir.join(playlist)).expect("audio media playlist");
        let paths: Vec<_> = media
            .lines()
            .filter(|line| {
                Path::new(line)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
            })
            .map(|line| dir.join(line))
            .collect();
        assert!(!paths.is_empty(), "empty {playlist}");
        let mut bytes = Vec::new();
        for path in paths {
            assert_eq!(
                probe_streams(&path)
                    .iter()
                    .map(|s| s.0.as_str())
                    .collect::<Vec<_>>(),
                ["audio"],
                "{}",
                path.display()
            );
            bytes.extend(std::fs::read(path).expect("rendition segment"));
        }
        let joined = dir.join(format!("{playlist}.ts"));
        std::fs::write(&joined, bytes).expect("joined audio");
        let other = if (frequency - 1000.0_f64).abs() < 1.0 {
            440.0
        } else {
            1000.0
        };
        assert_audible_tone(
            &decode_audio(&joined, "0:a:0"),
            playlist,
            frequency,
            other,
            4.0,
        );
    }
    let legacy = std::fs::read_to_string(dir.join("index.m3u8")).expect("legacy media playlist");
    assert!(legacy.contains("#EXTINF:"));
    assert!(!legacy.contains("#EXT-X-STREAM-INF"));
    let legacy_segment = legacy
        .lines()
        .find(|line| {
            Path::new(line)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
        })
        .expect("legacy TS URI");
    assert_eq!(
        probe_streams(&dir.join(legacy_segment))
            .iter()
            .map(|s| s.0.as_str())
            .collect::<Vec<_>>(),
        ["video", "audio", "audio"]
    );
}

#[tokio::test]
async fn tracks_mode_muxes_a_discrete_track_into_every_segment() {
    Box::pin(assert_two_audio_streams(
        "[outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"au-abc\"]",
    ))
    .await;
}

#[tokio::test]
async fn auto_mode_carries_program_plus_every_declared_track() {
    Box::pin(assert_two_audio_streams("[outputs.audio]\nmode = \"auto\"")).await;
}

#[tokio::test]
async fn program_mode_stays_single_audio_with_tracks_declared() {
    let (_guard, out_dir) = Box::pin(run("[outputs.audio]\nmode = \"program\"")).await;
    for seg in segments(&out_dir) {
        let streams = probe_streams(&seg);
        let kinds: Vec<&str> = streams.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(kinds, ["video", "audio"], "layout of {}", seg.display());
    }
}

#[tokio::test]
async fn tracks_mode_without_prog_carries_only_the_listed_track() {
    let (_guard, out_dir) = Box::pin(run(
        "[outputs.audio]\nmode = \"tracks\"\ntracks = [\"au-abc\"]",
    ))
    .await;
    for seg in segments(&out_dir) {
        let streams = probe_streams(&seg);
        let kinds: Vec<&str> = streams.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(kinds, ["video", "audio"], "layout of {}", seg.display());
        assert_eq!(streams[1].2, "eng", "the audio must be the track");
    }
}
