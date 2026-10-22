//! RT-11 close-out (ADR-0034): the **cli production command-drain** binds the
//! per-stream route commands to the canonical engine apply primitives, so a
//! `RouteVideo` / `RouteSubtitle` (and the legacy `SwapSource` alias) submitted on
//! the production command bus **actually applies in the real run** — not silently
//! dropped at the `#[non_exhaustive]` wildcard.
//!
//! Each property is proven against the **real composited frame** the running
//! libav\* pipeline publishes into its live program slot (sampled directly from
//! the engine's composited output — no tautology, no re-encode in the loop):
//!
//! 1. **`RouteVideo` re-points the cell in the run.** A run whose cell is
//!    config-bound to a DARK source is mid-run sent a `RouteVideo` onto a BRIGHT
//!    source through the production drain. After the take the composited tile is
//!    bright — the cell was actually re-pointed by the drain through the engine
//!    `RouteApplier` (`CompositorDrive::rebind_cell`), not a no-op.
//!
//! 2. **`SwapSource` (the back-compat alias) re-points identically.** The legacy
//!    command desugars to `RouteVideo{…,Video,Best}` and rides the SAME applier
//!    path, so it still re-points the cell — proving back-compat.
//!
//! 3. **`RouteSubtitle` re-points the layer in the run.** A run whose displayed
//!    layer carries an active cue is mid-run sent a `RouteSubtitle` re-pointing the
//!    layer to a SPARE source with no active cue, through the production drain's
//!    subtitle seam (the RT-10b `SubtitleRouteHandle`). After the take the tile's
//!    caption band CLEARS — the layer was actually re-pointed.
//!
//! All three ride one run each, driven by the REAL [`command_drain_with_seams`] the
//! binary wires (`run_pipeline_until_ctrl_c`), submitted on the REAL command bus.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing)]
// reason: test-only seconds→ns cue scaling on small bounded times (lossless in
// range) and the luma accumulation use `as`/float casts the integration-test lints
// don't auto-relax.
#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::process::Command as OsCommand;
use std::sync::Arc;
use std::time::Duration;

use multiview_cli::captions::CueStore;
use multiview_cli::control;
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::{program_slot, ProgramSlot};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::routing::StreamRef;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_core::stream::StreamKind;
use multiview_core::time::MediaTime;
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;
use multiview_ffmpeg::caption::CaptionCue;

/// The mean **luma** (Y, 0..255) over the centre of the composited frame — the
/// bright-vs-dark discriminator for the VIDEO re-point.
fn centre_luma(frame: &Nv12Image) -> f64 {
    let (w, h) = (frame.width(), frame.height());
    let (cx0, cy0, cx1, cy1) = (w / 4, h / 4, w * 3 / 4, h * 3 / 4);
    let mut sum = 0.0_f64;
    let mut n = 0.0_f64;
    for y in cy0..cy1 {
        for x in cx0..cx1 {
            if let Some((luma, _, _)) = frame.sample(x, y) {
                sum += f64::from(luma);
                n += 1.0;
            }
        }
    }
    if n == 0.0 {
        0.0
    } else {
        sum / n
    }
}

/// Extract a single PNG frame at `at_secs` of `media` (a playable container or HLS
/// playlist) into `png`.
fn extract_frame(media: &Path, at_secs: f64, png: &Path) {
    let status = OsCommand::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(media)
        .arg("-ss")
        .arg(format!("{at_secs}"))
        .args(["-frames:v", "1"])
        .arg(png)
        .status()
        .expect("spawn ffmpeg to extract a frame");
    assert!(
        status.success(),
        "frame extraction failed for {}",
        media.display()
    );
    assert!(png.exists(), "extracted frame PNG was not written");
}

/// The on-disk segment (`.ts`) paths the rolling HLS playlist currently
/// references, in playlist order (oldest → newest). Each `EXTINF` URI line is
/// resolved against the playlist's directory.
fn playlist_segments(playlist: &Path) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(playlist).expect("read HLS playlist");
    let dir = playlist.parent().unwrap_or_else(|| Path::new("."));
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|seg| dir.join(seg))
        .collect()
}

/// Extract the FIRST frame of segment `seg` into `png` (a positional, media-time-
/// independent capture — robust to live-pacing drift under CPU contention).
fn extract_first_frame(seg: &Path, png: &Path) {
    let status = OsCommand::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(seg)
        .args(["-frames:v", "1"])
        .arg(png)
        .status()
        .expect("spawn ffmpeg to extract first frame");
    assert!(
        status.success(),
        "first-frame extraction failed for {}",
        seg.display()
    );
    assert!(png.exists(), "extracted frame PNG was not written");
}

/// Decode a PNG to raw rgb24, returning `(width, height, bytes)`.
fn decode_rgb24(png: &Path) -> (u32, u32, Vec<u8>) {
    let dims = OsCommand::new("ffprobe")
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

    let out = OsCommand::new("ffmpeg")
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

/// Mean absolute per-pixel rgb difference between two PNG frames over the
/// bottom-centre caption band (the burned-subtitle detector).
fn band_absdiff(a: &Path, b: &Path) -> f64 {
    let (wa, ha, ra) = decode_rgb24(a);
    let (wb, hb, rb) = decode_rgb24(b);
    assert_eq!((wa, ha), (wb, hb), "frames differ in size");
    let (x0, y0, x1, y1) = (wa * 3 / 16, ha * 79 / 100, wa * 13 / 16, ha * 96 / 100);
    let mut sum = 0.0_f64;
    let mut count = 0.0_f64;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * wa + x) * 3) as usize;
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

/// A single-cell config bound to `cell_source`, with a HLS output (the live run's
/// rolling artifact) requesting LGPL `mpeg2video`. Three sources are declared: a
/// DARK `solid` (`in_dark`), a BRIGHT `solid` (`in_bright`), and a `test` source
/// `spare` (the subtitle breakaway target).
fn config_text(out_playlist: &Path, cell_source: &str) -> String {
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
id = "in_dark"
kind = "solid"
color = "#101010"

[[sources]]
id = "in_bright"
kind = "solid"
color = "#f0f0f0"

[[sources]]
id = "spare"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "{cell_source}"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        cell_source = cell_source,
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

/// Build a pipeline for `name` whose cell is bound to `cell_source`, returning it
/// plus its HLS playlist path.
fn build_pipeline(base: &Path, name: &str, cell_source: &str) -> (Pipeline, PathBuf) {
    let out_dir = base.join(name);
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&playlist, cell_source);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    let pipeline = Pipeline::build(&config).expect("build real pipeline");
    (pipeline, playlist)
}

/// A captured pair of composited frames: one BEFORE the route take, one AFTER.
struct Captured {
    before: Arc<Nv12Image>,
    after: Arc<Nv12Image>,
}

/// Drive a real serving run with the **production** command drain (subtitle seam
/// threaded), submit `command` mid-run on the REAL command bus, and capture the
/// composited frame just before and just after the take from the live program
/// slot. The composited frame is the engine's own output — the most direct proof
/// the drain re-pointed the live crosspoint.
async fn run_capturing(
    pipeline: &mut Pipeline,
    config: &MultiviewConfig,
    command: Command,
) -> Captured {
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot: ProgramSlot = program_slot();
    let (commands, command_rx) = command_bus(16);
    let stop = StopSignal::new();

    // The PRODUCTION drain, threaded with the run's live subtitle-route slot (so a
    // `RouteSubtitle` reaches the running pipeline's layer — the RT-10b seam).
    let subtitle_slot = pipeline.subtitle_route_slot();
    let live_hub = multiview_cli::live_sources::LiveSourceHub::start(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
    );
    let mut drain = control::command_drain_with_seams(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        subtitle_slot,
        pipeline.overlay_apply_slot(),
        live_hub.handle(),
        pipeline.player_mailboxes(),
    );

    // The capture/relay task: wait for the run to publish frames, snapshot the
    // BEFORE frame, submit the route command, let several ticks pass, snapshot the
    // AFTER frame, then stop the run. Wait-free reads of the live slot; the submit
    // is the non-blocking command bus — neither touches the run's `&mut`.
    let slot = Arc::clone(&preview_slot);
    let stop_for_relay = stop.clone();
    let relay = tokio::spawn(async move {
        // Wait until the run has published at least one composited frame.
        let mut before = None;
        for _ in 0..2_000 {
            if let Some(f) = slot.load_full() {
                before = Some(f);
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Hold a beat so the caption pipeline is warm and the BEFORE frame is well
        // clear of the take (the run paces to wall-clock, so ~1.5 s wall ≈ media
        // 1.5 s — within the 0..8 s cue window).
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let before = slot.load_full().or(before).expect("a before frame");

        // Submit the route command on the production bus.
        commands.try_submit(command).expect("submit route command");

        // Let the drain apply it and several composited frames roll past the seam.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let after = slot.load_full().expect("an after frame");

        stop_for_relay.stop();
        Captured { before, after }
    });

    let report = pipeline
        .run_until_serving(
            &stop,
            publisher.as_ref(),
            &preview_slot,
            move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
        )
        .await
        .expect("serving run");
    assert!(
        !report.faltered,
        "the route take must not falter the output"
    );
    relay.await.expect("relay/capture task")
}

/// (1) `RouteVideo` re-points the cell in the real run: a cell config-bound to the
/// DARK source is re-pointed mid-run to the BRIGHT source; the AFTER composited
/// frame is bright while the BEFORE frame was dark. One run; the take is the only
/// change.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn route_video_repoints_the_cell_in_the_running_pipeline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_text(&dir.path().join("v/index.m3u8"), "in_dark");
    let config = MultiviewConfig::load_from_toml(&toml).expect("config");
    let (mut pipeline, _playlist) = build_pipeline(dir.path(), "v", "in_dark");

    let command = Command::RouteVideo {
        op: OperationId::new(),
        cell: "cell_a".to_owned(),
        source: StreamRef::best("in_bright", StreamKind::Video),
    };
    let cap = run_capturing(&mut pipeline, &config, command).await;

    let before = centre_luma(&cap.before);
    let after = centre_luma(&cap.after);
    assert!(
        before < 80.0,
        "before the take the cell shows the DARK source (before luma {before:.1})"
    );
    assert!(
        after > 160.0,
        "after RouteVideo the cell shows the BRIGHT source (after luma {after:.1}); \
         the drain must re-point the cell through the engine RouteApplier, not drop it"
    );
}

/// (2) `SwapSource` (the back-compat alias) re-points the cell identically — it
/// desugars to `RouteVideo{…,Video,Best}` and rides the same applier path.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn swap_source_back_compat_still_repoints_the_cell() {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_text(&dir.path().join("s/index.m3u8"), "in_dark");
    let config = MultiviewConfig::load_from_toml(&toml).expect("config");
    let (mut pipeline, _playlist) = build_pipeline(dir.path(), "s", "in_dark");

    let command = Command::SwapSource {
        op: OperationId::new(),
        tile: "cell_a".to_owned(),
        source: "in_bright".to_owned(),
    };
    let cap = run_capturing(&mut pipeline, &config, command).await;

    assert!(
        centre_luma(&cap.before) < 80.0,
        "before the swap the cell is dark"
    );
    assert!(
        centre_luma(&cap.after) > 160.0,
        "SwapSource back-compat must still re-point the cell to the bright source"
    );
}

/// Drive a real serving run with the **production** command drain (subtitle seam
/// threaded), submit `command` ~2 s in on the REAL command bus, run ~5 s total
/// (within the 6-segment HLS window, so no segment is pruned), then stop. The
/// burned-overlay output (where subtitles live — NOT the pre-overlay program slot)
/// is the rolling HLS playlist, readable after the run.
async fn run_command_to_hls(pipeline: &mut Pipeline, config: &MultiviewConfig, command: Command) {
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot: ProgramSlot = program_slot();
    let (commands, command_rx) = command_bus(16);
    let stop = StopSignal::new();

    let subtitle_slot = pipeline.subtitle_route_slot();
    let live_hub = multiview_cli::live_sources::LiveSourceHub::start(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
    );
    let mut drain = control::command_drain_with_seams(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        subtitle_slot,
        pipeline.overlay_apply_slot(),
        live_hub.handle(),
        pipeline.player_mailboxes(),
    );

    let stop_for_relay = stop.clone();
    let relay = tokio::spawn(async move {
        // Submit the breakaway ~2 s in (well after the FIRST closed segment, which
        // is the pre-re-point reference), then let the run reach ~5.5 s and stop —
        // all within the 6-segment HLS window so every segment survives, and the
        // LAST closed segment is comfortably AFTER the re-point. Positional first/
        // last-segment extraction (below) is robust to CPU-contention pacing drift.
        tokio::time::sleep(Duration::from_secs(2)).await;
        commands.try_submit(command).expect("submit route command");
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        stop_for_relay.stop();
    });

    let report = pipeline
        .run_until_serving(
            &stop,
            publisher.as_ref(),
            &preview_slot,
            move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
        )
        .await
        .expect("serving run");
    assert!(
        !report.faltered,
        "the route take must not falter the output"
    );
    relay.await.expect("relay task");
}

/// (3) `RouteSubtitle` re-points the layer in the real run: the displayed layer's
/// active cue (burned into the band) is cleared by a mid-run breakaway onto a
/// SPARE source with no cue — so an EARLY frame's band carries the cue (differs
/// from a clean baseline) while a LATE frame's band is cleared (matches the
/// baseline). Captured from the burned HLS output of one run; the take is the only
/// change.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn route_subtitle_repoints_the_layer_in_the_running_pipeline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_text(&dir.path().join("sub/index.m3u8"), "in_dark");
    let config = MultiviewConfig::load_from_toml(&toml).expect("config");
    let (mut pipeline, playlist) = build_pipeline(dir.path(), "sub", "in_dark");

    // The displayed source (in_dark, the dark slate — a clean band to render onto)
    // carries a wide active cue; the spare has none.
    pipeline.attach_caption_store("in_dark", caption_store(0.0, 8.0, "BEFORE REPOINT"));
    pipeline.attach_caption_store("spare", Arc::new(CueStore::new()));

    let command = Command::RouteSubtitle {
        op: OperationId::new(),
        layer: "in_dark".to_owned(),
        source: StreamRef::best("spare", StreamKind::Subtitle),
    };
    run_command_to_hls(&mut pipeline, &config, command).await;

    // A clean baseline run (no caption store at all) — the empty-band reference.
    let (mut clean, clean_playlist) = build_pipeline(dir.path(), "sub_clean", "in_dark");
    let report = clean.run_for(100).await.expect("clean baseline run");
    assert_eq!(report.frames, 100);

    // Positional capture (robust to live-pacing drift): the EARLY frame is the
    // first frame of the FIRST surviving segment (pre-re-point, cue on); the LATE
    // frame is the first frame of the LAST segment (post-re-point, cue cleared).
    let segments = playlist_segments(&playlist);
    assert!(
        segments.len() >= 3,
        "the live run must have written several HLS segments (got {})",
        segments.len()
    );
    let first_seg = segments.first().expect("a first segment");
    let last_seg = segments.last().expect("a last segment");

    let early = dir.path().join("sub_early.png");
    let late = dir.path().join("sub_late.png");
    let clean_png = dir.path().join("sub_clean.png");
    extract_first_frame(first_seg, &early);
    extract_first_frame(last_seg, &late);
    extract_frame(&clean_playlist, 1.0, &clean_png);

    let early_vs_clean = band_absdiff(&early, &clean_png);
    let late_vs_clean = band_absdiff(&late, &clean_png);
    assert!(
        early_vs_clean > 2.0,
        "before the take the displayed cue is burned into the band \
         (early-vs-clean {early_vs_clean:.3})"
    );
    assert!(
        late_vs_clean < early_vs_clean / 4.0,
        "after RouteSubtitle the band must CLEAR (late-vs-clean {late_vs_clean:.3} << \
         early-vs-clean {early_vs_clean:.3}); the drain must drive the subtitle seam, not drop it"
    );
}
