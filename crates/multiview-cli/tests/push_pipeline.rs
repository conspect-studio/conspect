//! End-to-end test that the **real** `multiview run` pipeline now *runs* live push
//! outputs (RTMP / SRT) via the real [`PushSink`] — they are no longer skipped —
//! while preserving the program-output isolation guarantee (invariants #1/#10):
//! a push whose remote peer is unreachable must NOT fail the run or stop the
//! program's local file/HLS artifacts from being produced.
//!
//! Why this shape: a *delivered* push needs a listening peer. In CI / this sandbox
//! an RTMP or RTSP server is not running, and the SRT loopback handshake is
//! unavailable, so a live push cannot be pulled back and `ffprobe`d **through the
//! config-driven pipeline** here. The genuine end-to-end "the push delivers a real,
//! `ffprobe`-decodable MPEG-TS over a real socket" coverage therefore lives in
//! `multiview-output`'s `push_udp_ffprobe.rs` (the same `PushSink` this pipeline
//! wires, exercised over UDP-TS — the one push transport the sandbox permits).
//!
//! What this test proves about the *pipeline wiring*:
//!   1. A config carrying an RTMP push output builds and **runs** (the push is
//!      attempted via the real sink, not logged-and-skipped); and
//!   2. With the push peer unreachable, the run still completes and the HLS +
//!      `program.ts` are produced and `ffprobe`-valid — the dead remote consumer
//!      never back-pressured or failed the program.
//!
//! Licensing: `mpeg2video` (LGPL, in-tree) — no GPL escalation under `ffmpeg`.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;

/// Generate a short `testsrc` clip (LGPL `mpeg2video`) for the file source.
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

/// A 1x1 config with an HLS output (so a file + HLS sink are derived) **plus** an
/// RTMP push to a refused loopback port. The push exercises the real `PushSink`
/// wiring; port 1 on loopback is reliably refused so the connect fails fast and
/// the run is not held up.
fn config_text(clip: &Path, playlist: &Path, push_url: &str) -> String {
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

[[outputs]]
kind = "rtmp"
url = "{push_url}"
codec = "mpeg2video"
"##,
        clip = clip.display(),
        playlist = playlist.display(),
        push_url = push_url,
    )
}

#[tokio::test]
async fn pipeline_runs_a_push_output_and_an_unreachable_peer_does_not_break_the_program() {
    const TICKS: u64 = 50; // 2 s @ 25 fps

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    // Port 1 on the IPv6 loopback is reliably refused — the RTMP connect fails fast, so the
    // push sink returns promptly and the program's other outputs proceed.
    let push_url = "rtmp://[::1]:1/live/none";
    let toml = config_text(&clip, &playlist, push_url);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    // Sanity: the config really declares the push output (so the run below is
    // genuinely exercising the push-wiring path, not a no-op).
    assert_eq!(config.outputs.len(), 2, "config wires HLS + an RTMP push");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline with a push output");

    // The whole run must complete despite the unreachable push peer: invariant #1
    // (the output clock emits exactly N frames for N ticks) and #10 (a dead remote
    // consumer never back-pressures or fails the program).
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("the run completes even though the RTMP push peer is unreachable");
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(!report.faltered, "output must never falter (invariant #1)");

    // The program's LOCAL artifacts are produced and valid — the push being
    // undeliverable did not corrupt or skip them.
    let program = out_dir.join("program.ts");
    assert!(program.exists(), "program.ts must still be written");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts decodes to exactly N frames despite the failed push"
    );

    assert!(playlist.exists(), "HLS playlist must still be written");
    let playlist_text = std::fs::read_to_string(&playlist).expect("read playlist");
    assert!(playlist_text.starts_with("#EXTM3U"));
    assert!(playlist_text.contains("seg0.ts"));
    let seg0 = out_dir.join("seg0.ts");
    assert!(
        ffprobe_frame_count(&seg0) > 0,
        "first HLS segment must still decode"
    );
}

/// A config whose ONLY output is a push (no HLS/file). Before this wiring, the CLI
/// logged-and-skipped RTMP/SRT, so `build_outputs` produced an empty set and the
/// build failed with `NoOutput`. That a push-only pipeline now **builds and runs**
/// is the assertion-based proof the push is a real runnable output, not skipped.
/// (RTMP is used here only because a refused TCP connect fails fast and
/// deterministically; the SRT→`mpegts` mapping is covered in `multiview-output`.)
fn push_only_config_text(clip: &Path, push_url: &str) -> String {
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
kind = "rtmp"
url = "{push_url}"
codec = "mpeg2video"
"##,
        clip = clip.display(),
        push_url = push_url,
    )
}

#[tokio::test]
async fn a_push_only_config_is_runnable_now_that_push_is_wired() {
    const TICKS: u64 = 25; // 1 s @ 25 fps

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    // An RTMP push to a refused IPv6-loopback port: the TCP connect is refused fast (no
    // peer), so the push sink returns promptly and the run ends — the point of THIS
    // test is only that the build/run path exists for a push, not that bytes were
    // delivered (delivery is covered, over UDP-TS, by multiview-output's
    // `push_udp_ffprobe.rs`).
    let push_url = "rtmp://[::1]:1/live/none";
    let toml = push_only_config_text(&clip, push_url);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    assert_eq!(
        config.outputs.len(),
        1,
        "config wires exactly one push output"
    );

    // The keystone assertion: a push-only config BUILDS. If the push were still
    // skipped, `build_outputs` would be empty and this would be `Err(NoOutput)`.
    let mut pipeline =
        Pipeline::build(&config).expect("a push-only config must build (push is wired)");

    // And it RUNS to completion (the unreachable peer must not stall the clock).
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("the push-only run completes even though the peer is unreachable");
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(!report.faltered, "output must never falter (invariant #1)");
}
