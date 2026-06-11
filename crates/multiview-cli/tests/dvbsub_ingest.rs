//! End-to-end proof of **native in-pipeline DVB-sub (bitmap) caption ingest +
//! per-tile burn-in** (#36 Phase 2), features `ffmpeg` + `overlay`.
//!
//! It synthesizes a self-contained DVB-sub MPEG-TS (an in-tree `mpeg2video`
//! program + a `dvbsub` cue active from 1 s; the CLI cannot transcode text →
//! bitmap subtitle, so the fixture is built directly through libav by
//! `multiview_ffmpeg::test_fixtures`), wires it as a single-cell `file` source
//! with `captions = {mode="auto"}` + an HLS output, builds the [`Pipeline`],
//! drives a bounded run, and then **decodes frames from the produced
//! `program.ts`** and asserts the caption band is **bright (non-background)
//! inside the cue window** and **background before it** — i.e. the DVB-sub
//! bitmap is really burned into the tile, not merely surfaced as metadata.
//!
//! LGPL-clean: `mpeg2video` + in-tree `dvbsub`, never x264/x265.
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

/// A single-cell config over the DVB-sub fixture with `captions = auto` and an
/// HLS output (so `program.ts` is written). The canvas matches the fixture so
/// the tile is a 1:1 placement and the caption band maps predictably.
fn config_text(clip: &Path, playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 640
height = 480
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

/// The average luma (`YAVG`, 0..=255) of the caption-band crop of frame index
/// `frame` of `path`, via ffmpeg's `signalstats`. The crop is the horizontal
/// centre band over the lower part of the frame, where the DVB-sub cue burns in
/// (bottom-anchored, centred). Frames are selected by **index** (not `-ss`),
/// because `-ss` seeking on MPEG-TS is keyframe-imprecise.
fn caption_band_yavg(path: &Path, frame: u32) -> f64 {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "info",
            "-i",
            &path.display().to_string(),
            "-vf",
            // Select exactly frame `frame`, crop the caption band (w:h:x:y), then
            // measure YAVG; `metadata=print` writes it to stderr.
            &format!("select=eq(n\\,{frame}),crop=360:80:140:330,signalstats,metadata=print"),
            "-vsync",
            "0",
            "-frames:v",
            "1",
            "-an",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("spawn ffmpeg signalstats");
    let text = String::from_utf8_lossy(&out.stderr);
    // metadata=print writes lines like `lavfi.signalstats.YAVG=123.456`.
    let yavg = text
        .lines()
        .filter_map(|l| l.split("YAVG=").nth(1))
        .filter_map(|v| v.split_whitespace().next())
        .find_map(|v| v.parse::<f64>().ok());
    yavg.unwrap_or_else(|| panic!("no YAVG in ffmpeg output for frame {frame}:\n{text}"))
}

#[tokio::test]
async fn dvbsub_cue_burns_into_the_tile_during_its_window() {
    const TICKS: u64 = 125; // 5 s @ 25 fps — spans before (≈t0.4s) and inside (≈t2s).

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("dvbsub.ts");
    multiview_ffmpeg::test_fixtures::generate_dvbsub_ts(&clip).expect("generate dvbsub fixture");

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    assert_eq!(pipeline.source_count(), 1, "single source");

    let report = pipeline.run_for(TICKS).await.expect("bounded real run");
    assert_eq!(report.frames, TICKS, "N ticks → N frames (invariant #1)");
    assert!(!report.faltered, "output must never falter (invariant #1)");

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");

    // BEFORE the cue (frame 10 ≈ t0.4 s; the cue starts at ≈1.04 s): the caption
    // band is the program background — comparatively low luma.
    let before = caption_band_yavg(&program, 10);
    // INSIDE the cue window (frame 50 ≈ t2.0 s): the white DVB-sub band is burned
    // in — the band luma rises markedly.
    let inside = caption_band_yavg(&program, 50);

    assert!(
        before < 120.0,
        "before the cue, the caption band should be background, not the white \
         caption (YAVG {before})"
    );
    assert!(
        inside > before + 80.0,
        "inside the cue window the caption band must brighten markedly from the \
         burned-in DVB-sub bitmap (before {before}, inside {inside})"
    );
}
