//! End-to-end test of the libav\* `multiview run` pipeline (the `ffmpeg`
//! feature).
//!
//! This is the integration counterpart to the software smoke: it generates an
//! encoded input clip with the `ffmpeg` CLI, writes a small 2x2 config (canvas
//! 1280x720@25, a file source + test patterns, an HLS output), builds the
//! [`Pipeline`], drives a bounded run, and then **`ffprobe`s the produced
//! file** to confirm it is a playable multiview: the right codec/resolution,
//! the right frame count, and a decodable HLS segment. No tautologies — every
//! assertion is against an on-disk artifact.
//!
//! Licensing: the test asks for `mpeg2video` (LGPL, in-tree) so it passes under
//! the plain `ffmpeg` feature with no GPL escalation.
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

/// Run `ffprobe` and return the single requested stream entry value for the
/// first video stream of `path`.
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
    // mpegts may list the stream twice; take the first non-empty line.
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
    let value = {
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
            .to_owned()
    };
    value.parse().expect("frame count is an integer")
}

/// A small 2x2 config: a file source, two built-in test patterns, a fourth
/// file source, and an HLS output requesting the LGPL `mpeg2video` codec.
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
rows = ["1fr", "1fr"]
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "file"
path = "{clip}"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
kind = "file"
path = "{clip}"

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
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "in_c"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "in_d"

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
async fn pipeline_produces_a_playable_multiview_and_hls() {
    const TICKS: u64 = 50; // 2 s @ 25 fps

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build pipeline");
    assert_eq!(pipeline.source_count(), 4, "config wires four sources");
    // The LGPL-clean default encoder is mpeg2video (no GPL escalation in this
    // feature set).
    assert_eq!(pipeline.encoder_name(), "mpeg2video");

    let report = pipeline.run_for(TICKS).await.expect("bounded run");

    // Invariant #1: exactly N frames for N ticks, never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(!report.faltered, "output must never falter (invariant #1)");
    assert_eq!(report.canvas_width, 1280);
    assert_eq!(report.canvas_height, 720);

    // The single-file program is derived beside the playlist (encode-once).
    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert!(
        program.metadata().expect("stat program").len() > 0,
        "program.ts is empty"
    );

    // ffprobe the produced file: a playable mpeg2video 1280x720 multiview
    // with exactly the right number of frames.
    assert_eq!(ffprobe_v(&program, "codec_name"), "mpeg2video");
    assert_eq!(ffprobe_v(&program, "width"), "1280");
    assert_eq!(ffprobe_v(&program, "height"), "720");
    assert_eq!(ffprobe_v(&program, "r_frame_rate"), "25/1");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts must decode to exactly N frames"
    );

    // The HLS playlist exists, references segments, and the first segment is an
    // independently decodable file.
    assert!(playlist.exists(), "no HLS playlist written");
    let playlist_text = std::fs::read_to_string(&playlist).expect("read playlist");
    assert!(playlist_text.starts_with("#EXTM3U"));
    assert!(playlist_text.contains("#EXT-X-ENDLIST"));
    assert!(playlist_text.contains("seg0.ts"));

    let seg0 = out_dir.join("seg0.ts");
    assert!(seg0.exists(), "first HLS segment not written");
    assert!(
        ffprobe_frame_count(&seg0) > 0,
        "first HLS segment did not decode any frame"
    );
}

#[tokio::test]
async fn pipeline_holds_output_when_a_source_runs_out() {
    // The file source is a 2 s clip (50 frames). Driving 75 ticks must STILL
    // produce 75 frames: a source past its last frame holds its last-good frame
    // rather than stalling the output clock (invariant #1).
    const TICKS: u64 = 75;

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build pipeline");
    let report = pipeline.run_for(TICKS).await.expect("bounded run");

    assert_eq!(
        report.frames, TICKS,
        "output is independent of input exhaustion (invariant #1)"
    );
    assert!(!report.faltered);

    let program = out_dir.join("program.ts");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts decodes to N frames even though the file source had fewer"
    );
}

/// A 2x2 of every synthetic kind — bars, solid, an analog clock, a digital clock
/// — proving they are first-class sources the full pipeline renders in-process
/// (ADR-0027). Requires the `overlay` feature for the clock renderer.
#[cfg(feature = "overlay")]
fn synthetic_config_text(playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1
[canvas]
width = 480
height = 320
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b", "c d"]
[[sources]]
id = "bars"
kind = "bars"
[[sources]]
id = "slate"
kind = "solid"
color = "#cc3344"
[[sources]]
id = "clk_a"
kind = "clock"
face = "analog"
[[sources]]
id = "clk_d"
kind = "clock"
face = "digital"
twelve_hour = true
tz_offset_minutes = 600
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "bars"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "slate"
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "clk_a"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "clk_d"
[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist = playlist.display(),
    )
}

/// bars + solid + analog clock + digital clock, all first-class synthetic sources
/// (ADR-0027), render through the full libav pipeline and produce a playable
/// multiview that never falters — `solid` and `clock` used to be rejected at
/// ingest; now they are rendered in-process (no `ffmpeg testsrc` subprocess).
#[cfg(feature = "overlay")]
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn synthetic_sources_render_through_the_full_pipeline() {
    const TICKS: u64 = 30; // ~1.2 s @ 25 fps — long enough for a clock tick

    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = synthetic_config_text(&playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build pipeline");
    assert_eq!(pipeline.source_count(), 4, "four synthetic sources");
    let report = pipeline.run_for(TICKS).await.expect("bounded run");

    // Invariant #1: synthetic sources never stall the output clock.
    assert_eq!(
        report.frames, TICKS,
        "every synthetic source produced frames on every tick"
    );
    assert!(!report.faltered, "the output never faltered (invariant #1)");

    // The composited program is a playable file with exactly N frames.
    let program = out_dir.join("program.ts");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts decodes to N frames composited from bars + solid + two clocks"
    );
    assert_eq!(ffprobe_v(&program, "codec_name"), "mpeg2video");
}

/// A small 2x2 of built-in `bars` sources (no external network) + an HLS output
/// — the pipeline synthesizes each source, composites, and encodes locally.
fn serving_config_text(playlist: &Path) -> String {
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
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
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
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "in_c"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "in_d"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist = playlist.display(),
    )
}

/// The integration the product is built around: a **single** run of the full
/// libav\* pipeline both *ingests/composites/encodes* AND *serves* the control
/// plane + live program preview. Proves ingestion, processing, output, and
/// management are one process — and that serving never stalls the output clock
/// (inv #1) nor back-pressures the engine (inv #10).
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn pipeline_serves_control_api_and_live_preview_while_ingesting() {
    use std::sync::Arc;
    use std::time::Duration;

    use multiview_cli::control;
    use multiview_cli::preview::{program_slot, CliPreviewProvider};
    use multiview_compositor::pipeline::Nv12Image;
    use multiview_control::{command_bus, EngineStateSnapshot, SharedPreview};
    use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
    use multiview_events::Event;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = serving_config_text(&playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build pipeline");

    // The engine's outbound publisher + the live-preview slot, both shared
    // read-only with the control plane (invariant #10).
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = program_slot();
    let provider: SharedPreview = Arc::new(CliPreviewProvider::new(
        Arc::clone(&preview_slot),
        multiview_cli::live_sources::shared_stores(pipeline.preview_stores()),
    ));
    let (commands, _command_rx) = command_bus(8);
    let stop = StopSignal::new();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // IPv6-first: the serve path must bind the IPv6 loopback `[::1]`.
    let (addr, server) = control::bind_and_serve(
        "[::1]:0",
        &config,
        Arc::clone(&publisher),
        commands,
        Arc::clone(&provider),
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");

    // Client: GET the public OpenAPI doc while the pipeline ingests/encodes, let
    // a few frames produce, then raise the engine's stop signal.
    let stop_for_client = stop.clone();
    let client = tokio::spawn(async move {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /api/v1/openapi.json HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        stop_for_client.stop();
        String::from_utf8_lossy(&buf).into_owned()
    });

    // The full pipeline ingests + composites + encodes + serves concurrently;
    // this returns once the client raises stop. A no-op command drain (no live
    // reconfiguration in this test).
    let drain = |_: &mut CompositorDrive<Nv12Image>| {};
    let report = pipeline
        .run_until_serving(&stop, publisher.as_ref(), &preview_slot, drain)
        .await
        .expect("pipeline serving run");

    assert!(
        !report.faltered,
        "the output clock must not falter while the API + preview are served"
    );
    assert!(
        report.frames >= 1,
        "the pipeline produced frames while serving"
    );

    let body = client.await.unwrap();
    let status = body.lines().next().unwrap_or_default();
    assert_eq!(
        status.split_whitespace().nth(1),
        Some("200"),
        "openapi status line: {status:?}"
    );
    assert!(body.contains("openapi"), "served an OpenAPI document");

    // The shared publisher carries the engine's compact state snapshot — the
    // bridge the dashboard reads from the wait-free latest-state slot.
    let snap = publisher
        .state
        .latest()
        .expect("the engine published a state snapshot");
    assert_eq!(
        snap.as_ref()["canvas"]["width"].as_u64(),
        Some(320),
        "the snapshot carries the canvas geometry"
    );

    // The SAME run filled the live-preview slot, and the provider encodes it to a
    // JPEG on demand — the program preview the WebUI renders, served straight
    // from the ingesting pipeline (not a separate process).
    assert!(
        preview_slot.load_full().is_some(),
        "the pipeline published a program frame into the live-preview slot"
    );
    let jpeg = provider
        .program_jpeg(80)
        .expect("the program preview encodes to a JPEG");
    assert!(
        jpeg.len() > 2 && jpeg[0] == 0xFF && jpeg[1] == 0xD8,
        "the program preview is a JPEG (SOI marker present)"
    );

    let _ = shutdown_tx.send(());
    let _ = server.await;
}

/// A 2x2 config with a single **file** source bound to a cell plus three
/// synthetic `test` sources — so exactly one input (`in_file`) has a container
/// to probe an inventory from (RT-3).
fn inventory_config_text(clip: &Path, out_playlist: &Path) -> String {
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
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b", "c d"]

[[sources]]
id = "in_file"
kind = "file"
path = "{clip}"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_file"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "in_c"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "in_d"

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

/// RT-3 end-to-end: a real `multiview run` of the libav\* pipeline probes each
/// file source's elementary-stream inventory at build time (off the
/// output-clock thread), threads it into the published `EngineStateSnapshot`
/// (where the control plane's `GET /inputs/{id}/streams` reads it), and emits
/// **exactly one** `input.streams` delta per probed input on the `inputs`
/// realtime lane. Synthetic sources (no container) contribute no inventory.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn pipeline_publishes_input_stream_inventory_and_one_delta() {
    use std::sync::Arc;

    use multiview_cli::preview::program_slot;
    use multiview_compositor::pipeline::Nv12Image;
    use multiview_control::EngineStateSnapshot;
    use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal, TryRecvError};
    use multiview_events::Event;

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = inventory_config_text(&clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build pipeline");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let preview_slot = program_slot();
    // Subscribe BEFORE the run so the run-start `input.streams` deltas are
    // captured (the engine publishes through the drop-oldest broadcast; a slow
    // subscriber never back-pressures it — inv #10).
    let mut sub = publisher.subscribe();

    let stop = StopSignal::new();
    // A bounded run: stop after the first batch of frames so the test is fast and
    // deterministic. The inventory deltas are emitted at run start, well within
    // this budget.
    let stop_for_timer = stop.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        stop_for_timer.stop();
    });

    // The SAME publisher the subscription watches is threaded through the run, so
    // the run-start `input.streams` deltas land on `sub`.
    let drain = |_: &mut CompositorDrive<Nv12Image>| {};
    pipeline
        .run_until_serving(&stop, publisher.as_ref(), &preview_slot, drain)
        .await
        .expect("pipeline serving run");
    timer.await.unwrap();

    // The published snapshot carries the file source's inventory under
    // `inputs.in_file.streams`, and NOT the synthetic ones (no container).
    let snap = publisher
        .state
        .latest()
        .expect("the engine published a state snapshot");
    let streams = &snap.as_ref()["inputs"]["in_file"]["streams"];
    let inventory: multiview_core::stream::StreamInventory =
        serde_json::from_value(streams.clone()).expect("a valid StreamInventory in the snapshot");
    assert_eq!(inventory.input_id.as_deref(), Some("in_file"));
    assert!(
        inventory.video().count() >= 1,
        "the probed inventory exposes the clip's video stream"
    );
    assert!(
        snap.as_ref()["inputs"].get("in_b").is_none(),
        "a synthetic source has no container, so no inventory is published"
    );

    // Exactly ONE `input.streams` delta was emitted for the one probed input.
    let mut input_streams_for_file = 0usize;
    loop {
        match sub.try_recv() {
            Ok(seq) => {
                if let Event::InputStreams(is) = seq.event.as_ref() {
                    if is.input_id == "in_file" {
                        input_streams_for_file += 1;
                        assert!(
                            is.inventory.video().count() >= 1,
                            "the delta carries the same video stream"
                        );
                    }
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            // A lag would mean drop-oldest overflow; the 256-deep buffer is far
            // larger than the handful of events here, so this never fires — keep
            // looping to drain whatever is still buffered.
            Err(TryRecvError::Lagged(_)) => {}
        }
    }
    assert_eq!(
        input_streams_for_file, 1,
        "exactly one input.streams delta per probed input (no duplicate re-emit)"
    );
}
