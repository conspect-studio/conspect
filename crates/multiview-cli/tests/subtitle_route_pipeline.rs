//! RT-10b end-to-end: the run path **samples the per-layer `SubtitleLayer`** each
//! output tick and a **re-point takes effect** in the running pipeline (features
//! `ffmpeg` + `overlay`).
//!
//! Two properties are proven against real on-disk artifacts (no tautology):
//!
//! 1. **The run samples + renders the subtitle layer.** A run with a native
//!    caption cue store attached to the displayed source burns that source's
//!    active cue into its tile; an otherwise-identical run with no caption store
//!    leaves the tile's bottom-centre band clean. The cue therefore reaches actual
//!    output pixels THROUGH the [`SubtitleRouter`](multiview_cli::captions::SubtitleRouter)
//!    (the run no longer samples the per-source store directly).
//!
//! 2. **A re-point is effective in the run.** Mid-run, the control plane re-points
//!    the displayed layer to a DIFFERENT source whose cue is NOT active — through
//!    the live [`Pipeline::subtitle_route_handle`]. After the re-point the tile's
//!    caption band CLEARS (CLEAR-on-switch + the new source has nothing active), so
//!    a late frame's band is clean while an early frame's band carried the cue.
//!    Both come from the SAME run, so the re-point — not a second pipeline — is the
//!    only difference.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing)]
// reason: test-only seconds→ns scaling on small bounded cue times (lossless in
// range) and the rgb-diff accumulation use `as`/float casts the integration-test
// lints don't auto-relax.
#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use multiview_cli::captions::CueStore;
use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_core::time::MediaTime;
use multiview_ffmpeg::caption::CaptionCue;

/// Extract a single PNG frame at `at_secs` of `video` into `png` (output seek for
/// frame-exact decode on a sparse-keyframe MPEG-TS).
fn extract_frame(video: &Path, at_secs: f64, png: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(video)
        .arg("-ss")
        .arg(format!("{at_secs}"))
        .args(["-frames:v", "1"])
        .arg(png)
        .status()
        .expect("spawn ffmpeg to extract a frame");
    assert!(status.success(), "frame extraction failed");
    assert!(png.exists(), "extracted frame PNG was not written");
}

/// Decode a PNG to raw rgb24 with `ffmpeg`, returning `(width, height, bytes)`.
fn decode_rgb24(png: &Path) -> (u32, u32, Vec<u8>) {
    let dims = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(png)
        .output()
        .expect("ffprobe dims");
    let dims = String::from_utf8_lossy(&dims.stdout);
    let dims = dims.trim();
    let (w_str, h_str) = dims.split_once('x').expect("WxH dims");
    let w: u32 = w_str.trim().parse().expect("width");
    let h: u32 = h_str.trim().parse().expect("height");

    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(png)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .output()
        .expect("ffmpeg decode rgb24");
    assert!(out.status.success(), "ffmpeg rgb24 decode failed");
    let bytes = out.stdout;
    let expect_len = usize::try_from(w).unwrap() * usize::try_from(h).unwrap() * 3;
    assert_eq!(bytes.len(), expect_len, "rgb24 buffer size mismatch");
    (w, h, bytes)
}

/// A rectangular region of an rgb24 frame.
struct Region {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

/// The byte offset of pixel `(x, y)` in a `width`-wide rgb24 buffer.
fn px_index(x: u32, y: u32, width: u32) -> usize {
    let x = usize::try_from(x).unwrap();
    let y = usize::try_from(y).unwrap();
    let width = usize::try_from(width).unwrap();
    (y * width + x) * 3
}

/// Mean absolute per-pixel rgb difference between two frames over `region`.
fn region_mean_absdiff(a: &Path, b: &Path, region: &Region) -> f64 {
    let (wa, ha, ra) = decode_rgb24(a);
    let (wb, hb, rb) = decode_rgb24(b);
    assert_eq!((wa, ha), (wb, hb), "frames differ in size");
    let _ = hb;
    let x1 = region.x1.min(wa);
    let y1 = region.y1.min(ha);
    let mut sum = 0.0_f64;
    let mut count = 0.0_f64;
    for y in region.y0..y1 {
        for x in region.x0..x1 {
            let i = px_index(x, y, wa);
            for k in 0..3 {
                sum += f64::from(ra[i + k].abs_diff(rb[i + k]));
                count += 1.0;
            }
        }
    }
    if count == 0.0 {
        0.0
    } else {
        sum / count
    }
}

/// A single built-in `test` source filling a 1x1 grid, with an HLS output
/// requesting LGPL `mpeg2video` (also anchors the self-contained `program.ts`).
fn config_text(out_playlist: &Path) -> String {
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

/// A native caption cue store carrying one text cue over `[start, end)` seconds.
fn caption_store(start_s: f64, end_s: f64, line: &str) -> Arc<CueStore> {
    let store = Arc::new(CueStore::new());
    let start = MediaTime::from_nanos((start_s * 1e9) as i64);
    let end = MediaTime::from_nanos((end_s * 1e9) as i64);
    let cue =
        CaptionCue::try_text(start, end, vec![line.to_owned()], None).expect("valid text cue");
    store.publish(start, end, cue);
    store
}

/// Build a pipeline for `name`, returning it plus its `program.ts` path.
fn build_pipeline(base: &Path, name: &str) -> (Pipeline, PathBuf) {
    let out_dir = base.join(name);
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    let pipeline = Pipeline::build(&config).expect("build real pipeline");
    (pipeline, out_dir.join("program.ts"))
}

/// The bottom-centre caption band of the (full-canvas) tile, and a control band
/// higher up that the caption never touches.
fn band() -> Region {
    Region {
        x0: 120,
        y0: 380,
        x1: 520,
        y1: 460,
    }
}
fn control() -> Region {
    Region {
        x0: 120,
        y0: 120,
        x1: 520,
        y1: 180,
    }
}

/// (1) The run SAMPLES + RENDERS the subtitle layer: a native caption store
/// attached to the displayed source burns its cue into the tile (the band differs
/// from a no-caption run); the control band is unchanged.
#[tokio::test]
async fn run_renders_the_subtitle_layers_active_cue() {
    const TICKS: u64 = 50; // 2 s @ 25 fps.

    let dir = tempfile::tempdir().expect("tempdir");

    // Run A: a caption store attached to the displayed source, active the whole run.
    let (mut with_caps, prog_with) = build_pipeline(dir.path(), "with_caps");
    with_caps.attach_caption_store("in_a", caption_store(0.0, 2.0, "MULTIVIEW ROUTE TEST"));
    let report = with_caps
        .run_for(TICKS)
        .await
        .expect("bounded run with caps");
    assert_eq!(report.frames, TICKS, "N ticks -> N frames");
    assert!(
        !report.faltered,
        "RT-10b sampling must not falter the output"
    );

    // Run B: identical pipeline, NO caption store.
    let (mut no_caps, prog_no) = build_pipeline(dir.path(), "no_caps");
    let report = no_caps.run_for(TICKS).await.expect("bounded run no caps");
    assert_eq!(report.frames, TICKS);

    // Same media instant (1.0 s) from both.
    let frame_on = dir.path().join("on.png");
    let frame_off = dir.path().join("off.png");
    extract_frame(&prog_with, 1.0, &frame_on);
    extract_frame(&prog_no, 1.0, &frame_off);

    let band_diff = region_mean_absdiff(&frame_on, &frame_off, &band());
    let control_diff = region_mean_absdiff(&frame_on, &frame_off, &control());
    assert!(
        control_diff < 0.01,
        "the control band must be identical between runs (got {control_diff:.4})"
    );
    assert!(
        band_diff > 2.0,
        "the subtitle layer's active cue must reach the tile band through the router \
         (band diff {band_diff:.3} vs control {control_diff:.3})"
    );
}

/// (2) A RE-POINT is effective IN THE RUN: mid-run the displayed layer is
/// re-pointed (via the live `subtitle_route_handle`) to a source whose cue is not
/// active, so the tile's caption band CLEARS — a late frame's band is clean while
/// an early frame's band carried the cue. One run; the re-point is the only change.
///
/// The run borrows the pipeline mutably for its whole duration, so the re-point is
/// issued from a sibling task that holds a clone of the pipeline's **shared**
/// re-point slot ([`Pipeline::subtitle_route_slot`]). The run publishes its live
/// handle into that slot at drive start; the sibling polls the slot, then requests
/// the breakaway EARLY in the run — wait-free, never touching the run's `&mut`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subtitle_repoint_takes_effect_in_the_running_pipeline() {
    // A long run so the early frame (cue present) and the late frame (after the
    // re-point) are well separated and the mid-run re-point lands between them.
    const TICKS: u64 = 200; // 8 s @ 25 fps.

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut pipeline, program) = build_pipeline(dir.path(), "repoint");

    // The displayed source carries a wide cue (active the whole run). A SPARE
    // source carries NO active cue. Re-pointing the displayed layer to the spare
    // therefore clears the band.
    pipeline.attach_caption_store("in_a", caption_store(0.0, 8.0, "BEFORE REPOINT"));
    pipeline.attach_caption_store("spare", Arc::new(CueStore::new()));

    // A clone of the pipeline's shared re-point slot — readable concurrently with
    // the (mutably-borrowing) run. The run publishes the live handle into it at
    // drive start.
    let slot = pipeline.subtitle_route_slot();

    // The re-point relay: poll the slot until the run has published its handle, then
    // wait until the run is PAST the early frame's media time (the run paces to
    // wall-clock, ~25 fps, so ~3 s of wall-clock ≈ media 3 s — comfortably after the
    // 0.4 s early frame and before the 7.0 s late frame) and request the breakaway
    // ("in_a" layer -> empty "spare"). Runs concurrently with the run on a second
    // worker thread.
    let relay = tokio::spawn(async move {
        // Bounded poll: the handle is published at drive start (well within this).
        let mut handle = None;
        for _ in 0..2_000 {
            if let Some(h) = slot.load_full() {
                handle = Some(h);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let Some(handle) = handle else {
            return false;
        };
        // Land the re-point between the early (0.4 s) and late (7.0 s) frames.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        handle.request_repoint("in_a", "spare");
        true
    });

    let report = pipeline.run_for(TICKS).await.expect("bounded run");
    assert_eq!(report.frames, TICKS, "N ticks -> N frames");
    assert!(!report.faltered, "the re-point must not falter the output");

    let requested = relay.await.expect("relay task");
    assert!(
        requested,
        "the run must have published its subtitle route handle so the breakaway could be requested"
    );

    // An EARLY frame (0.4 s): before the re-point lands, the displayed cue is on.
    // A LATE frame (7.0 s): well after the early re-point, the layer reads the
    // empty spare → the band is cleared.
    let early = dir.path().join("early.png");
    let late = dir.path().join("late.png");
    extract_frame(&program, 0.4, &early);
    extract_frame(&program, 7.0, &late);

    // Baseline: a separate run with NO caption store at all — the clean-band
    // reference for both bands.
    let (mut clean, clean_prog) = build_pipeline(dir.path(), "repoint_clean");
    let report = clean.run_for(TICKS).await.expect("clean run");
    assert_eq!(report.frames, TICKS);
    let clean_png = dir.path().join("clean.png");
    extract_frame(&clean_prog, 0.4, &clean_png);

    // The early band carried the cue (differs from the clean baseline); the late
    // band is cleared (matches the clean baseline) — the re-point took effect.
    let early_vs_clean = region_mean_absdiff(&early, &clean_png, &band());
    let late_vs_clean = region_mean_absdiff(&late, &clean_png, &band());

    assert!(
        early_vs_clean > 2.0,
        "before the re-point the displayed cue must be burned in (early-vs-clean band diff \
         {early_vs_clean:.3})"
    );
    assert!(
        late_vs_clean < early_vs_clean / 4.0,
        "after the re-point to an empty source the band must CLEAR — late-vs-clean band diff \
         {late_vs_clean:.3} must be far below the early-vs-clean {early_vs_clean:.3}"
    );
}
