//! Regression guard for **BUG-2: live ingest must STREAM, never buffer to EOF**
//! (the `ffmpeg` feature).
//!
//! The defect was that the pipeline decoded *every* frame of *every* source into
//! a `Vec` inside `Pipeline::build` before the output clock ever started. For
//! a finite file that merely wasted time and memory; for a **live** stream (which
//! never emits EOF) it hung forever and `--duration`/`--ticks` never took effect.
//!
//! These tests pin the fixed contract without needing the network:
//!
//! 1. [`build_does_not_predecode_the_whole_source`] — `build` must return
//!    promptly even for a source with far more frames than the bounded run will
//!    consume. If `build` ever again decodes the whole source up front, decoding
//!    a multi-second clip would dominate and the elapsed-time assertion fails.
//!    (For a true never-ending live source the *old* code would hang here
//!    forever; a finite-but-large clip reproduces the same "decoded eagerly in
//!    build" signature deterministically and offline.)
//! 2. [`bounded_run_completes_promptly_and_emits_exactly_n_frames`] — a bounded
//!    `run_for(N)` produces exactly `N` composited frames (output independent of
//!    input length, invariant #1) and tears ingest down so the call returns.
//!
//! No tautologies: assertions are against the real `Pipeline` behavior and
//! the on-disk artifact.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;

/// Generate a `secs`-second `720p` `testsrc` clip (LGPL `mpeg2video`, in-tree).
/// A long, large clip (many megapixels of frames) makes "decoded eagerly in
/// `build`" — decode + per-frame scale + `Nv12Image` allocation via the safe
/// wrappers — unmistakably slower than merely opening the container.
fn generate_clip(path: &Path, secs: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=1280x720:rate=25:duration={secs}"),
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-g",
            "25",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI to generate the input clip");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(path.exists(), "input clip was not written");
}

/// Count decodable video frames in `path` via `ffprobe -count_frames`.
fn ffprobe_frame_count(path: &Path) -> u64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe -count_frames");
    assert!(out.status.success(), "ffprobe -count_frames failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(','))
        .find(|l| !l.is_empty() && *l != "N/A")
        .unwrap_or("0")
        .parse()
        .expect("frame count is an integer")
}

/// A 1x1 config over a single file source at a small canvas (so the CPU
/// reference compositor keeps up in CI) with an HLS output.
fn config_text(clip: &Path, out_playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 320
height = 240
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

#[tokio::test]
async fn build_does_not_predecode_the_whole_source() {
    // A 20 s 720p clip = 500 frames. The OLD pipeline decoded ALL of them
    // (decode + scale + allocate, on the calling thread) inside `build`, which
    // takes seconds. The fix defers decoding to per-source ingest threads
    // started by `run_*`, so `build` opens nothing and only resolves the plan —
    // it must return well under a generous bound regardless of clip length.
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("long.ts");
    generate_clip(&clip, 20);

    let playlist = dir.path().join("out").join("index.m3u8");
    let toml = config_text(&clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let started = Instant::now();
    let pipeline = Pipeline::build(&config).expect("build real pipeline");
    let elapsed = started.elapsed();

    assert_eq!(pipeline.source_count(), 1, "config wires one source");
    // Decoding 250 frames eagerly would take much longer than this; a streaming
    // build (no decode) returns near-instantly. The bound is deliberately loose
    // (CI headroom) yet far below the eager-decode cost of a 10 s clip.
    assert!(
        elapsed < Duration::from_secs(3),
        "build must not pre-decode the whole source (took {elapsed:?}); ingest is streamed"
    );
}

#[tokio::test]
async fn bounded_run_completes_promptly_and_emits_exactly_n_frames() {
    // The file source is a 20 s clip (500 frames), but we only run 25 ticks
    // (1 s). A streaming pipeline samples the store per tick and STOPS after 25
    // ticks — it never waits for the source to finish, so the call returns and
    // exactly 25 frames are produced (invariant #1: output is independent of how
    // much input remains).
    const TICKS: u64 = 25;

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("long.ts");
    generate_clip(&clip, 20);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");

    let started = Instant::now();
    let report = pipeline.run_for(TICKS).await.expect("bounded real run");
    let elapsed = started.elapsed();

    assert_eq!(
        report.frames, TICKS,
        "N ticks must produce exactly N frames"
    );
    assert!(!report.faltered, "output must never falter (invariant #1)");

    // The bounded run must complete in time comparable to its OUTPUT duration
    // (1 s at 25 fps) plus encode overhead — NOT the source's 10 s length. A
    // pipeline that drained the whole source first would take ~10 s+; the
    // streaming pipeline stops at the tick budget.
    assert!(
        elapsed < Duration::from_secs(8),
        "bounded run must stop at the tick budget, not drain the source (took {elapsed:?})"
    );

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts must decode to exactly N frames"
    );
}
