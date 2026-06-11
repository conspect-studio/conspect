//! End-to-end proof that a config source with **embedded CEA-608** captions
//! drives a real [`CaptionCue`] through the **cli reader-wiring** into the
//! per-source [`CueStore`] (SUR-3b/3c), features `ffmpeg` + `overlay`.
//!
//! This is deliberately a **wiring** test, not a decoder unit test: it builds a
//! real [`Pipeline`] from a `captions = {mode="embedded_cc", field="cc1"}` source
//! over a self-contained `mpeg2video` MPEG-TS fixture whose video frames carry the
//! caption as `AV_FRAME_DATA_A53_CC` side data, drives a bounded run (which spawns
//! the real per-source ingest thread → `open_and_stream` → `StreamVideoDecoder` →
//! A53-side-data extraction → `cc_dec`), and then asserts the recovered text cue
//! landed in the source's `CueStore` at the right media instant. The A53 side data
//! must be pulled off the **raw** decoded video frame before NV12 conversion, so a
//! pass proves that whole seam is wired, not just that the decoder can decode.
//!
//! LGPL-clean: `mpeg2video` + the linked `cc_dec`, never x264/x265.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_core::time::MediaTime;
use multiview_ffmpeg::caption::CaptionCue;
use multiview_ffmpeg::test_fixtures::{generate_a53_cc_ts, A53_CAPTION_TEXT, A53_FPS};

/// A single-cell config over the A53 fixture selecting embedded CC field CC1.
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
mode = "embedded_cc"
field = "cc1"

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

#[tokio::test]
async fn embedded_cc_source_drives_a_text_cue_into_the_cue_store() {
    // The fixture runs a few frames past the last caption word so the EOC frame
    // (which is the one cc_dec emits the cue on) is well inside this run.
    const TICKS: u64 = 60; // ≈2.4 s @ 25 fps.

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("a53.ts");
    generate_a53_cc_ts(&clip).expect("generate A53 fixture");

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    assert_eq!(pipeline.source_count(), 1, "single source");

    // The pipeline must have wired a native caption store for the embedded-CC
    // source at build time — this is the wiring under test.
    let store = pipeline
        .caption_store_for("in_a")
        .expect("the embedded-CC source must have a native caption store wired");

    let report = pipeline.run_for(TICKS).await.expect("bounded real run");
    assert_eq!(report.frames, TICKS, "N ticks → N frames (invariant #1)");
    assert!(!report.faltered, "output must never falter (invariant #1)");

    // The recovered embedded caption must be in the store. The fixture emits the
    // pop-on caption around the trailing words; scan the run's media window for the
    // recovered text cue (the cue store is sampled by absolute media time).
    let fps = i64::from(A53_FPS);
    let mut found: Option<CaptionCue> = None;
    for frame in 0..TICKS {
        let frame = i64::try_from(frame).expect("tick fits i64");
        let ns = frame.saturating_mul(1_000_000_000) / fps;
        if let Some(cue) = store.active_at(MediaTime::from_nanos(ns)) {
            found = Some(cue);
            break;
        }
    }

    match found.expect("an embedded-CC text cue must reach the cue store via the wiring") {
        CaptionCue::Text { text, start, end } => {
            assert_eq!(
                text.lines,
                vec![A53_CAPTION_TEXT.to_owned()],
                "the wiring recovered the known embedded caption text"
            );
            assert!(
                end.as_nanos() > start.as_nanos(),
                "the cue has a bounded, positive on-screen window"
            );
        }
        other => panic!("expected a text cue from embedded CC, got {other:?}"),
    }
}
