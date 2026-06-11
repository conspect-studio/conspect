//! End-to-end test that opting into program audio produces a **dual-stream**
//! container — one video stream plus one (silent) AAC audio stream (AUD-4,
//! feature `ffmpeg`).
//!
//! It builds the real libav* pipeline over a minimal multiview, calls
//! [`Pipeline::enable_program_audio`], drives a short bounded run to a file, and
//! `ffprobe`s the produced `program.ts` to confirm it carries exactly ONE video
//! stream and exactly ONE audio stream. The audio is silence (no audio sources
//! are wired in this slice), but it is a real AAC elementary stream, proving the
//! encode-once-mux-many path now fans both the video AND the program-audio
//! packets into the muxer. The video stays exactly N frames for N ticks, so the
//! off-hot-path audio encode never falters the output (invariant #1). No
//! tautology: every assertion is against the on-disk artifact.
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
// reason: the non-silence proof generates a synthetic tone (index<->float casts,
// exact for the small ranges used) and computes an RMS over decoded f32 — test-only.

use std::path::Path;
use std::process::Command;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;

/// Count the elementary streams of `kind` (`"a"` audio, `"v"` video) in `path`,
/// de-duplicated by stream index so the MPEG-TS double-listing (PMT + PES) does
/// not inflate the count.
fn ffprobe_stream_count(path: &Path, kind: &str) -> usize {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            kind,
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed for {}",
        path.display()
    );
    let mut indices: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(',').to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices.len()
}

/// The AAC `codec_name` of the first audio stream of `path`.
fn ffprobe_audio_codec(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe audio codec failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .map(|l| l.trim_end_matches(','))
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// A 1x1 config: a single built-in `test` source plus one HLS output (which
/// also anchors the self-contained `program.ts`), requesting the LGPL
/// `mpeg2video` video codec.
fn config_text(out_playlist: &Path) -> String {
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
kind = "test"

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
"##,
        playlist = out_playlist.display(),
    )
}

#[tokio::test]
async fn program_audio_produces_a_dual_stream_container() {
    const TICKS: u64 = 30; // 1.2 s @ 25 fps — short but enough to ffprobe.

    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    pipeline.enable_program_audio();
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("bounded real run with program audio");

    // Invariant #1: the program-audio encode runs OFF the hot path, so the video
    // is still exactly N frames for N ticks and never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(
        !report.faltered,
        "program-audio encode must not falter the output"
    );

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert!(
        program.metadata().expect("stat program").len() > 0,
        "program.ts is empty"
    );

    // The dual-stream proof: exactly one video stream AND exactly one audio
    // stream, the audio being a real AAC elementary stream.
    assert_eq!(
        ffprobe_stream_count(&program, "v"),
        1,
        "program.ts must carry exactly one video stream"
    );
    assert_eq!(
        ffprobe_stream_count(&program, "a"),
        1,
        "program.ts must carry exactly one audio stream"
    );
    assert_eq!(
        ffprobe_audio_codec(&program),
        "aac",
        "the program-audio stream must be AAC"
    );
}

/// Whether the `ffmpeg` CLI is available (used only to build the deterministic
/// fixture clip + decode the output audio for the RMS proof; never on the data
/// plane). Mirrors the gate in `multiview-audio`'s `decode_thread.rs`.
fn ffmpeg_cli_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Build a deterministic **audio-bearing** fixture clip at `path`: a short H.264-
/// free MPEG-2 video carrying a loud 1 kHz tone in an AAC track, packaged via the
/// `ffmpeg` CLI (LGPL-clean — `mpeg2video` + native `aac`, never x264/x265).
///
/// `lavfi` synthesises both streams so the fixture needs no network and is
/// reproducible: a `testsrc` video and a `sine` audio at full amplitude, muxed
/// into an MPEG-TS the pipeline opens as a `file` source.
fn build_av_fixture(path: &Path, seconds: f64) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("testsrc=size=320x240:rate=25:duration={seconds}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency=1000:sample_rate=48000:duration={seconds}"
        ))
        .args([
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg to build the A/V fixture");
    assert!(status.success(), "ffmpeg failed to build the A/V fixture");
}

/// Decode every audio sample of `path` to interleaved `f32` via the `ffmpeg` CLI
/// and return the mean-square energy (RMS²) — `> 0` proves the audio is not
/// silence. Decoding the on-disk artifact (not the in-memory bus) makes the
/// proof genuinely end-to-end.
fn output_audio_mean_square(path: &Path) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map", "a:0", "-f", "f32le", "-ac", "2", "-ar", "48000", "pipe:1",
        ])
        .output()
        .expect("spawn ffmpeg to decode output audio");
    assert!(
        out.status.success(),
        "ffmpeg failed to decode output audio from {}",
        path.display()
    );
    let bytes = out.stdout;
    let n = bytes.len() / 4;
    assert!(n > 0, "no audio samples decoded from {}", path.display());
    let mut energy = 0.0f64;
    for chunk in bytes.chunks_exact(4) {
        let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        energy += f64::from(v) * f64::from(v);
    }
    energy / (n as f64)
}

/// Decode `path`'s audio to **mono** `f32` at 48 kHz via the `ffmpeg` CLI and
/// estimate the dominant tone frequency (Hz) by counting zero-crossings. A clean
/// sine of frequency `f` crosses zero `2f` times per second, so
/// `crossings / (2 · seconds)` recovers `f`. This is the coarse frequency proof
/// that the program audio is a ~1 kHz tone, not just non-silent noise — decoded
/// from the on-disk artifact so it is genuinely end-to-end.
fn output_audio_dominant_hz(path: &Path) -> f64 {
    const RATE: usize = 48_000;
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map", "a:0", "-f", "f32le", "-ac", "1", "-ar", "48000", "pipe:1",
        ])
        .output()
        .expect("spawn ffmpeg to decode output audio (mono)");
    assert!(out.status.success(), "ffmpeg failed to decode mono audio");
    let samples: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert!(
        samples.len() > RATE,
        "too few samples for a frequency estimate"
    );
    // Skip the encoder's leading priming/ramp by ignoring the first 0.25 s, then
    // count sign changes through zero on the remaining steady tone. A small
    // dead-band rejects denormal jitter around zero.
    let start = RATE / 4;
    let mut crossings = 0usize;
    let mut prev_sign = 0i32;
    for &s in &samples[start..] {
        let sign = if s > 1e-4 {
            1
        } else if s < -1e-4 {
            -1
        } else {
            prev_sign
        };
        if sign != 0 && prev_sign != 0 && sign != prev_sign {
            crossings += 1;
        }
        if sign != 0 {
            prev_sign = sign;
        }
    }
    let seconds = (samples.len() - start) as f64 / RATE as f64;
    crossings as f64 / (2.0 * seconds)
}

/// A 1x1 config whose single source is a **file** at `clip` (an audio-bearing
/// MPEG-TS) plus one HLS output anchoring `program.ts`, requesting `mpeg2video`.
fn file_source_config_text(clip: &Path, out_playlist: &Path) -> String {
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
"##,
        clip = clip.display(),
        playlist = out_playlist.display(),
    )
}

/// The load-bearing AUD-2 proof: a source whose container carries a real audio
/// tone, decoded on its own thread into the per-source `AudioStore`, routed onto
/// the program bus, mixed per tick, and AAC-encoded into the output — so the
/// program audio is **not silence** (mean-square energy `> 0`), end-to-end.
///
/// This is the gap AUD-2 closes: before the per-source `audio_decode_loop` was
/// spawned + its store routed onto the `ProgramBus`, the bus mixed zero sources
/// and the AAC track was silence. The assertion decodes the on-disk `program.ts`
/// audio, so it proves the whole decode→store→bus→mix→encode→mux chain, not just
/// the decode unit. The video still produces exactly N frames for N ticks
/// (invariant #1: the off-hot-path audio decode never paces or falters output).
#[tokio::test]
async fn program_audio_carries_decoded_source_audio_not_silence() {
    const TICKS: u64 = 30; // 1.2 s @ 25 fps.

    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping decoded-audio non-silence test");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("fixture.ts");
    build_av_fixture(&clip, 2.0);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = file_source_config_text(&clip, &playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    pipeline.enable_program_audio();
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("bounded real run with program audio over an audio-bearing source");

    // Invariant #1: the audio decode + program encode run OFF the hot path, so the
    // video is still exactly N frames for N ticks and never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(
        !report.faltered,
        "decoded-source program audio must not falter the output"
    );

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");

    // Dual-stream is still required (the AUD-4 guarantee holds).
    assert_eq!(
        ffprobe_stream_count(&program, "a"),
        1,
        "program.ts must carry exactly one audio stream"
    );

    // The AUD-2 proof: the program audio is the decoded source tone, NOT silence.
    let ms = output_audio_mean_square(&program);
    assert!(
        ms > 1e-5,
        "program audio is silent (mean-square {ms}); the per-source decode loop \
         did not fill the store / route onto the bus"
    );
}

/// A 1x1 config whose single source is a synthetic **`bars`** card (no external
/// media at all) plus one HLS output anchoring `program.ts`, requesting
/// `mpeg2video`.
fn bars_source_config_text(out_playlist: &Path) -> String {
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
"##,
        playlist = out_playlist.display(),
    )
}

/// The load-bearing AUD-5 proof: a `bars` synthetic source — with NO external
/// media — produces a real **1 kHz line-up tone** on the program bus, AAC-encoded
/// into the output. Before AUD-5 the bars source contributed no audio store, so
/// the program was silence; now the synth render path publishes the tone into the
/// source's `AudioStore`, which is routed onto the `ProgramBus` exactly like a
/// decoded source's audio (reusing AUD-2's routing).
///
/// The proof decodes the on-disk `program.ts` audio (not the in-memory bus): it
/// must be non-silent (mean-square `> 0`) AND its dominant zero-crossing
/// frequency must be ~1 kHz (the SMPTE/EBU line-up tone), so this exercises the
/// whole tone→store→bus→mix→encode→mux chain end-to-end. The video still produces
/// exactly N frames for N ticks (invariant #1: the tone publish is off the hot
/// path and never paces or falters output).
#[tokio::test]
async fn bars_source_program_audio_is_a_1khz_tone() {
    const TICKS: u64 = 75; // 3 s @ 25 fps — enough for a stable frequency estimate.

    if !ffmpeg_cli_available() {
        eprintln!("ffmpeg CLI unavailable; skipping bars-tone program-audio test");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = bars_source_config_text(&playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    pipeline.enable_program_audio();
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("bounded real run with bars-tone program audio");

    // Invariant #1: the tone publish runs OFF the hot path, so the video is still
    // exactly N frames for N ticks and never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(
        !report.faltered,
        "bars-tone program audio must not falter the output"
    );

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert_eq!(
        ffprobe_stream_count(&program, "a"),
        1,
        "program.ts must carry exactly one audio stream"
    );

    // Non-silence: the bars source now contributes the line-up tone.
    let ms = output_audio_mean_square(&program);
    assert!(
        ms > 1e-5,
        "bars program audio is silent (mean-square {ms}); the bars tone was not \
         published into its AudioStore / routed onto the bus"
    );

    // ~1 kHz: the dominant zero-crossing frequency proves it is the reference tone,
    // not arbitrary energy. AAC coding + the encoder ramp blur it slightly, so a
    // generous +/-120 Hz window keeps the test robust while still distinguishing a
    // 1 kHz tone from silence, noise, or a wildly wrong frequency.
    let hz = output_audio_dominant_hz(&program);
    assert!(
        (hz - 1000.0).abs() < 120.0,
        "bars program audio dominant frequency {hz} Hz must be the ~1 kHz line-up tone"
    );
}
