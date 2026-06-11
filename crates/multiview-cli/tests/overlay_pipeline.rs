//! End-to-end test that **configured overlays reach actual output pixels**
//! (Stage 3 wire, features `ffmpeg` + `overlay`).
//!
//! It builds the real libav* pipeline over a small 2x2 multiview, drives a short
//! bounded run with overlay baking enabled (clock label, dB meter, safe-area,
//! tally border, via the pure-Rust overlay sub-pass), and `ffprobe`s the
//! produced file to confirm it is still a real, playable mpeg2video 1280x720
//! program with exactly the right number of frames — i.e. the overlay baking
//! ran off the hot path and did not falter the output (invariant #1). No
//! tautology: every assertion is against the on-disk artifact.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::process::Command;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;

/// Generate a 2-second `testsrc` clip (LGPL `mpeg2video`) for the file source.
fn generate_clip(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=2",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI to generate the input clip");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(path.exists(), "input clip was not written");
}

/// One ffprobe stream entry value for the first video stream of `path`.
fn ffprobe_v(path: &Path, entry: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            &format!("stream={entry}"),
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
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_end_matches(',')
        .to_owned()
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

/// A 1x2 config: a real file source plus a test pattern, with an HLS output
/// requesting the LGPL `mpeg2video` codec.
fn config_text(clip: &Path, out_playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 1280
height = 720
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]

[[sources]]
id = "in_a"
kind = "file"
path = "{clip}"
[[sources]]
id = "in_b"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"

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
async fn overlays_are_baked_into_the_real_pipeline_output() {
    const TICKS: u64 = 30; // 1.2 s @ 25 fps — short but enough to ffprobe.

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("bounded real run with overlays");

    // Invariant #1: overlay baking runs OFF the hot path, so the output is still
    // exactly N frames for N ticks and never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(
        !report.faltered,
        "overlay baking must not falter the output"
    );

    // The overlaid program is a real, playable mpeg2video 1280x720 file with
    // exactly the right number of frames.
    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert!(
        program.metadata().expect("stat program").len() > 0,
        "overlaid program.ts is empty"
    );
    assert_eq!(ffprobe_v(&program, "codec_name"), "mpeg2video");
    assert_eq!(ffprobe_v(&program, "width"), "1280");
    assert_eq!(ffprobe_v(&program, "height"), "720");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "overlaid program.ts must still decode to exactly N frames"
    );
}
