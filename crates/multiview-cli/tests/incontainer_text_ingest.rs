//! End-to-end proof that a config source carrying an **in-container TEXT subtitle
//! track** (`mov_text` in MP4, `ass`/SSA in MKV) drives a real [`CaptionCue`]
//! through the **cli reader-wiring** into the per-source [`CueStore`] (SUR-3b/3c),
//! features `ffmpeg` + `overlay`.
//!
//! The in-container DVB-sub (bitmap) route already shipped; this proves the TEXT
//! sibling of that route — a muxed `ass`/`subrip`/`mov_text` stream is decoded on
//! the source's own ingest thread (no second demux) and its text cues reach the
//! store. The fixtures are built with the `ffmpeg` CLI (which CAN mux text
//! subtitles, unlike the DVB-sub bitmap fixture), keeping the codecs LGPL-clean
//! (`mpeg2video` video + linked text decoders, never x264/x265).
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
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
use multiview_core::time::MediaTime;
use multiview_ffmpeg::caption::CaptionCue;

/// The caption text both fixtures burn into the subtitle track.
const CUE_TEXT: &str = "HELLO SUBTITLE";

/// Build a clip at `path` with a `mpeg2video` video and one TEXT subtitle track
/// (`subtitle_codec`, e.g. `mov_text`/`ass`) carrying [`CUE_TEXT`] on screen from
/// 1 s to 3 s, in container format `container` (e.g. `mp4`/`matroska`).
fn build_text_subtitle_clip(path: &Path, srt: &Path, subtitle_codec: &str, container: &str) {
    std::fs::write(srt, "1\n00:00:01,000 --> 00:00:03,000\nHELLO SUBTITLE\n").expect("write srt");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=4",
            "-i",
            &srt.display().to_string(),
            "-map",
            "0:v",
            "-map",
            "1:0",
            "-c:v",
            "mpeg2video",
            "-c:s",
            subtitle_codec,
            "-f",
            container,
            &path.display().to_string(),
        ])
        .status()
        .expect("spawn ffmpeg to build the text-subtitle fixture");
    assert!(status.success(), "ffmpeg fixture build failed");
}

/// A single-cell config over `clip` with `captions = {mode="auto"}` (auto selects
/// the in-container text subtitle track) and an HLS output.
fn config_text(clip: &Path, playlist: &Path) -> String {
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
[sources.captions]
mode = "auto"

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
        playlist = playlist.display(),
    )
}

/// Drive the real pipeline over `clip` and return the text cue that reached the
/// source's caption store inside the cue window (≈t1.5 s), or `None`.
async fn cue_reaching_store(clip: &Path, out_dir: &Path) -> Option<CaptionCue> {
    const TICKS: u64 = 75; // 3 s @ 25 fps — spans the 1..3 s cue window.
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    assert_eq!(pipeline.source_count(), 1, "single source");
    let store = pipeline
        .caption_store_for("in_a")
        .expect("the in-container text source must have a native caption store wired");

    let report = pipeline.run_for(TICKS).await.expect("bounded real run");
    assert_eq!(report.frames, TICKS, "N ticks → N frames (invariant #1)");
    assert!(!report.faltered, "output must never falter (invariant #1)");

    // The cue is on screen 1..3 s; sample at ≈1.5 s.
    store.active_at(MediaTime::from_nanos(1_500_000_000))
}

fn assert_text_cue(cue: Option<CaptionCue>, what: &str) {
    match cue.unwrap_or_else(|| panic!("a {what} text cue must reach the cue store via the wiring"))
    {
        CaptionCue::Text { text, start, end } => {
            assert!(
                text.lines.iter().any(|l| l.contains(CUE_TEXT)),
                "the {what} wiring recovered the known caption text, got {:?}",
                text.lines
            );
            assert!(
                end.as_nanos() > start.as_nanos(),
                "the {what} cue has a bounded, positive on-screen window"
            );
        }
        other => panic!("expected a text cue from {what}, got {other:?}"),
    }
}

#[tokio::test]
async fn in_container_mov_text_drives_a_text_cue_into_the_cue_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("movtext.mp4");
    let srt = dir.path().join("sub.srt");
    build_text_subtitle_clip(&clip, &srt, "mov_text", "mp4");

    let cue = cue_reaching_store(&clip, &dir.path().join("out_movtext")).await;
    assert_text_cue(cue, "mov_text");
}

#[tokio::test]
async fn in_container_ass_drives_a_text_cue_into_the_cue_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("ass.mkv");
    let srt = dir.path().join("sub.srt");
    build_text_subtitle_clip(&clip, &srt, "ass", "matroska");

    let cue = cue_reaching_store(&clip, &dir.path().join("out_ass")).await;
    assert_text_cue(cue, "ass");
}
