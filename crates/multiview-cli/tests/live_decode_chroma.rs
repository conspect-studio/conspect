//! ADR-W018 level 2 — chroma parity of a LIVE-ADDED decode vs a STARTUP decode
//! of the SAME clip (the on-hardware Defect A reproduction: a runtime-added
//! HLS/h264 source showed magenta/green corruption — a U/V inversion — while
//! startup sources on the same run were colour-correct).
//!
//! The probe decodes one strongly-chromatic clip (solid red: Cr ≫ Cb) through
//! BOTH constructions on one real run:
//!
//! * the **startup** path (`Pipeline::build` → `IngestSupervisor::start`), the
//!   source bound to a cell (decode at cell geometry);
//! * the **live** path (command-bus `UpsertSource` → drain → `LiveSourceHub` →
//!   `Pipeline::live_ingest_spawner` → the same `spawn_ingest_producer`), the
//!   source unbound (decode at canvas geometry — the runtime-add shape).
//!
//! A U/V swap inverts the plane means (red: mean(Cr) ≈ 240 vs mean(Cb) ≈ 90),
//! so asserting `mean(Cr) − mean(Cb) > 60` on EACH store — and that the two
//! stores' means agree — pins "live == startup behaviourally" at plane level.
#![cfg(feature = "ffmpeg")]
// reason: `*_cb` / `*_cr` are the canonical Cb/Cr chroma-plane names; the
// startup/live pairs intentionally read alike because they measure the SAME
// statistic on two constructions — renaming for distance would obscure the
// parity the test asserts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::similar_names
)]

use std::path::Path;
use std::process::Command as OsCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::control;
use multiview_cli::live_sources::{shared_stores, LiveSourceHub, SharedStores};
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::program_slot;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{
    CompositorDrive, EnginePublisher, MonotonicTimeSource, StopSignal, TimeSource,
};
use multiview_events::Event;

/// Generate a solid RED clip (strong chroma asymmetry: Cb ≈ 90, Cr ≈ 240 in
/// BT.601 limited) long enough to stay decoding for the observation window.
fn generate_red_clip(path: &Path) {
    let status = OsCommand::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=320x240:rate=25:duration=20",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-qscale:v",
            "2",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI to generate the red clip");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
}

/// Two-cell config: cell a bound to the STARTUP file source (so it decodes at
/// cell geometry), cell b bound to a dark solid. The live-added source stays
/// unbound (it decodes at canvas geometry — the runtime-add shape).
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]

[[sources]]
id = "in_startup"
kind = "file"
path = "{clip}"

[[sources]]
id = "in_dark"
kind = "solid"
color = "#101010"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_startup"

[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_dark"

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

/// Mean Cb and Cr over an NV12 image's interleaved UV plane.
fn uv_means(image: &Nv12Image) -> (f64, f64) {
    let uv = image.uv_plane();
    let mut cb_sum = 0_f64;
    let mut cr_sum = 0_f64;
    let mut pairs = 0_f64;
    for pair in uv.chunks_exact(2) {
        if let [cb, cr] = pair {
            cb_sum += f64::from(*cb);
            cr_sum += f64::from(*cr);
            pairs += 1.0;
        }
    }
    assert!(pairs > 0.0, "UV plane must be non-empty");
    (cb_sum / pairs, cr_sum / pairs)
}

/// Wait (bounded) for `id`'s store in `stores` to publish a frame, then return
/// a snapshot of it.
fn wait_frame(stores: &SharedStores, id: &str, deadline: Duration) -> Arc<Nv12Image> {
    let clock = MonotonicTimeSource::new();
    let end = Instant::now() + deadline;
    loop {
        if let Some(store) = stores.load().get(id) {
            let now = multiview_core::time::MediaTime::from_nanos(clock.now_nanos());
            let read = store.read_at(now);
            if let Some(frame) = read.frame() {
                return Arc::clone(frame);
            }
        }
        assert!(
            Instant::now() < end,
            "store {id} never published a frame within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// PLANE-LEVEL CHROMA PARITY (Defect A pin): a live-added decode of the SAME
/// clip must publish frames whose U/V plane statistics match the startup
/// decode's — a U/V swap inverts the means and fails this loudly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_added_decode_matches_startup_uv_statistics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("red.ts");
    generate_red_clip(&clip);

    let toml = config_text(&clip, &dir.path().join("out/index.m3u8"));
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = program_slot();
    let (commands, command_rx) = command_bus(32);
    let stop = StopSignal::new();

    // The hub with the REAL ingest spawner — the binary's full-pipeline wiring.
    let stores = shared_stores(pipeline.preview_stores());
    let hub = LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        Arc::clone(&stores),
        Some(pipeline.live_ingest_spawner()),
    );
    let mut drain = control::command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );

    let clip_path = clip.display().to_string();
    let probe_stores = Arc::clone(&stores);
    let driver_stop = stop.clone();
    let driver = tokio::spawn(async move {
        // Live-add the SAME clip under a new id; it stays unbound (canvas
        // geometry), exactly the runtime-add shape from the hardware run.
        commands
            .try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source: Box::new(
                    serde_json::from_value(serde_json::json!({
                        "id": "in_live", "kind": "file", "path": clip_path
                    }))
                    .expect("valid file source"),
                ),
            })
            .expect("submit upsert");

        let (startup_frame, live_frame) = tokio::task::block_in_place(|| {
            let startup = wait_frame(&probe_stores, "in_startup", Duration::from_secs(15));
            let live = wait_frame(&probe_stores, "in_live", Duration::from_secs(15));
            (startup, live)
        });
        driver_stop.stop();
        (startup_frame, live_frame)
    });

    pipeline
        .run_until_serving(
            &stop,
            publisher.as_ref(),
            &preview_slot,
            move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
        )
        .await
        .expect("serving run with a live-added decode");
    let (startup_frame, live_frame) = driver.await.expect("driver task");
    hub.shutdown();

    let (startup_cb, startup_cr) = uv_means(&startup_frame);
    let (live_cb, live_cr) = uv_means(&live_frame);

    // Red ⇒ Cr ≫ Cb on BOTH stores. A U/V swap inverts a store's means.
    assert!(
        startup_cr - startup_cb > 60.0,
        "startup decode chroma is wrong (cb={startup_cb:.1}, cr={startup_cr:.1})"
    );
    assert!(
        live_cr - live_cb > 60.0,
        "LIVE-ADDED decode chroma is inverted/corrupt vs startup \
         (live cb={live_cb:.1} cr={live_cr:.1}; startup cb={startup_cb:.1} cr={startup_cr:.1})"
    );
    // And the two constructions agree at plane level (same clip, same decode).
    assert!(
        (live_cb - startup_cb).abs() < 8.0 && (live_cr - startup_cr).abs() < 8.0,
        "live-added decode diverges from the startup decode of the same clip: \
         live (cb={live_cb:.1}, cr={live_cr:.1}) vs startup (cb={startup_cb:.1}, cr={startup_cr:.1})"
    );
}
