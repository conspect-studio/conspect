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

/// `(power at 440 Hz, power at 1 kHz)` of a decoded stream: which input it
/// carries, robust to the phase jumps a faster-than-real-time file decode can
/// leave in its store.
fn tone_powers(path: &Path, stream: &str) -> (f64, f64) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map", stream, "-f", "f32le", "-ac", "1", "-ar", "48000", "pipe:1",
        ])
        .output()
        .expect("spawn ffmpeg decode");
    assert!(out.status.success(), "decode of {stream} failed");
    let samples: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert!(samples.len() > 48_000, "too few samples in {stream}");
    // Sum local powers instead of one whole-run phasor: AAC segment boundaries
    // reset phase, so a correct tone can cancel across the joined TS stream.
    let powers = samples[12_000..]
        .chunks(4_800)
        .filter(|chunk| chunk.len() == 4_800);
    powers.fold((0.0, 0.0), |(track, program), chunk| {
        (
            track + tone_power(chunk, f64::from(TRACK_HZ)),
            program + tone_power(chunk, 1000.0),
        )
    })
}

fn dominant_hz(path: &Path, stream: &str) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map", stream, "-f", "f32le", "-ac", "1", "-ar", "48000", "pipe:1",
        ])
        .output()
        .expect("spawn ffmpeg decode");
    assert!(out.status.success(), "decode of {stream} failed");
    let samples: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert!(samples.len() > 48_000, "too few samples in {stream}");
    // Measure over VOICED 10 ms windows only: a file source's decode thread can
    // trail the clock on a loaded host, and the store silence-fills that span;
    // silence carries no zero-crossings and would bias the estimate low. The
    // voiced fraction is reported so a mostly-silent track still fails.
    let body = &samples[12_000..];
    let mut crossings = 0usize;
    let mut voiced = 0usize;
    for window in body.chunks(480) {
        let energy: f32 = window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32;
        if energy < 1e-6 {
            continue;
        }
        voiced += window.len();
        let mut prev = 0i32;
        for &s in window {
            let sign = if s > 1e-4 {
                1
            } else if s < -1e-4 {
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
    let fraction = voiced as f64 / body.len() as f64;
    assert!(
        fraction > 0.5,
        "{stream} is mostly silent ({fraction:.3} voiced)"
    );
    crossings as f64 / (2.0 * voiced as f64 / 48_000.0)
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
    let (_guard, out_dir) = run(output_audio).await;
    let segs = segments(&out_dir);
    assert!(!segs.is_empty(), "no segments written");
    let mut joined = Vec::new();
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
        joined.extend(std::fs::read(seg).expect("read segment"));
    }
    let all = out_dir.join("joined.bin");
    std::fs::write(&all, joined).expect("write joined");
    let program_hz = dominant_hz(&all, "0:a:0");
    assert!(
        (program_hz - 1000.0).abs() < 50.0,
        "program must stay the 1 kHz mix, got {program_hz}"
    );
    let (track_440, track_1k) = tone_powers(&all, "0:a:1");
    assert!(
        track_440 > 1e4,
        "muted -18 dB program route must retain a clean, audible discrete track"
    );
    assert!(
        track_440 > 10.0 * track_1k,
        "track must carry the discrete 440 Hz input, not the 1 kHz program \
         (440 Hz power {track_440:.3e}, 1 kHz power {track_1k:.3e})"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Full manifest traversal and real decode belong in one end-to-end assertion.
async fn master_resolves_playable_distinct_audio_only_renditions() {
    let (_guard, dir) = run("[outputs.audio]\nmode = \"auto\"").await;
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
        let (track, program) = tone_powers(&joined, "0:a:0");
        let (expected, other) = if (frequency - 1000.0_f64).abs() < 1.0 {
            (program, track)
        } else {
            (track, program)
        };
        assert!(
            expected > 1e4,
            "{playlist}: expected {frequency} Hz is inaudible"
        );
        assert!(
            expected > 10.0 * other,
            "{playlist}: expected {frequency} Hz power {expected:.3e}, other {other:.3e}"
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
    assert_two_audio_streams("[outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"au-abc\"]")
        .await;
}

#[tokio::test]
async fn auto_mode_carries_program_plus_every_declared_track() {
    assert_two_audio_streams("[outputs.audio]\nmode = \"auto\"").await;
}

#[tokio::test]
async fn program_mode_stays_single_audio_with_tracks_declared() {
    let (_guard, out_dir) = run("[outputs.audio]\nmode = \"program\"").await;
    for seg in segments(&out_dir) {
        let streams = probe_streams(&seg);
        let kinds: Vec<&str> = streams.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(kinds, ["video", "audio"], "layout of {}", seg.display());
    }
}

#[tokio::test]
async fn tracks_mode_without_prog_carries_only_the_listed_track() {
    let (_guard, out_dir) = run("[outputs.audio]\nmode = \"tracks\"\ntracks = [\"au-abc\"]").await;
    for seg in segments(&out_dir) {
        let streams = probe_streams(&seg);
        let kinds: Vec<&str> = streams.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(kinds, ["video", "audio"], "layout of {}", seg.display());
        assert_eq!(streams[1].2, "eng", "the audio must be the track");
    }
}
