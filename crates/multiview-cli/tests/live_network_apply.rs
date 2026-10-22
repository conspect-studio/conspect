//! ADR-W018 **level 2** — live ADD / EDIT / REMOVE of network/file sources on
//! the running libav\* pipeline (the `ffmpeg` feature).
//!
//! The command drain applies `UpsertSource` for a decoded kind at the frame
//! boundary (store + route key registration only) and hands the heavy spawn to
//! the live-source hub, whose ingest spawner builds the plan with the **same**
//! `ingest_plan_for` construction and runs the **same** supervised
//! `ingest_loop` the startup path runs — one uniform ingest path, never a
//! second-quality copy.
//!
//! Proven here end-to-end against a real run:
//!
//! * a live-added FILE source (a deterministic local fixture — the same libav
//!   open/decode/scale/publish path every network kind rides) reaches **LIVE**,
//!   observed via the engine's own `tile.state` events;
//! * a live REMOVE returns the bound cell to **`NO_SIGNAL`** (slate);
//! * rapid add/remove churn of the decoded source never falters the output
//!   clock (invariants #1 + #10).
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::process::Command as OsCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::control;
use multiview_cli::live_sources::{shared_stores, LiveSourceHub};
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::program_slot;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;

/// Generate a bright `testsrc` clip (LGPL `mpeg2video` in MPEG-TS) long enough
/// to stay decoding for the whole observation window.
fn generate_clip(path: &Path) {
    let status = OsCommand::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=20",
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

/// A single-cell config bound to a dark solid source, HLS output (mpeg2video).
fn config_text(out_playlist: &Path) -> String {
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
id = "in_dark"
kind = "solid"
color = "#101010"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_dark"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist = out_playlist.display(),
    )
}

/// Wait (bounded) for a `tile.state` event moving `input` to `want`.
fn wait_state(
    sub: &mut multiview_engine::EventSubscription<Event>,
    input: &str,
    want: multiview_events::LifecycleState,
    deadline: Duration,
) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if Instant::now() > end {
            return false;
        }
        match sub.try_recv() {
            Ok(envelope) => {
                if let Event::TileState(ts) = envelope.event.as_ref() {
                    if ts.input.as_deref() == Some(input) && ts.to == want {
                        return true;
                    }
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// REALTIME PROOF (ADR-W018 level 2): on a real libav pipeline run, a
/// live-added file source's tile reaches LIVE via the uniform hub-spawned
/// `ingest_loop`, a live remove returns it to `NO_SIGNAL`, and rapid add/remove
/// churn of the decoded source never falters the output clock.
// reason: one end-to-end proof that drives the full add -> live -> remove ->
// slate -> churn lifecycle on a REAL pipeline in a single observation window;
// splitting it would duplicate the costly pipeline build/run scaffold and
// scatter one coherent realtime assertion across helpers.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_added_file_source_goes_live_then_remove_slates_never_faltering() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("clip.ts");
    generate_clip(&clip);

    let toml = config_text(&dir.path().join("out/index.m3u8"));
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = program_slot();
    let (commands, command_rx) = command_bus(32);
    let stop = StopSignal::new();

    // The hub with the REAL ingest spawner — exactly the binary's wiring on the
    // full-pipeline run path (the seam that flips network kinds to live).
    let hub = LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        shared_stores(pipeline.preview_stores()),
        Some(pipeline.live_ingest_spawner()),
    );
    let mut drain = control::command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );

    let mut sub = publisher.subscribe();
    let clip_path = clip.display().to_string();
    let driver_stop = stop.clone();
    let driver = tokio::spawn(async move {
        let upsert = |id: &str| Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(
                serde_json::from_value(
                    serde_json::json!({ "id": id, "kind": "file", "path": clip_path }),
                )
                .expect("valid file source"),
            ),
        };

        // Live-add the file source and bind the cell to it.
        commands
            .try_submit(upsert("live_clip"))
            .expect("submit upsert");
        commands
            .try_submit(Command::SwapSource {
                op: OperationId::new(),
                tile: "cell_a".to_owned(),
                source: "live_clip".to_owned(),
            })
            .expect("submit swap");

        let went_live = tokio::task::block_in_place(|| {
            wait_state(
                &mut sub,
                "live_clip",
                multiview_events::LifecycleState::Live,
                Duration::from_secs(15),
            )
        });

        // Live REMOVE: the bound cell must return to the slate (NO_SIGNAL).
        commands
            .try_submit(Command::RemoveSource {
                op: OperationId::new(),
                id: "live_clip".to_owned(),
            })
            .expect("submit remove");
        let went_dark = tokio::task::block_in_place(|| {
            wait_state(
                &mut sub,
                "live_clip",
                multiview_events::LifecycleState::NoSignal,
                Duration::from_secs(15),
            )
        });

        // Decoded-source churn: rapid add/remove cycles (real spawn + teardown
        // through the hub) must ride the bounded seams without faltering the
        // clock — the level-2 extension of the churn soak.
        for n in 0..3_u32 {
            let id = format!("churn{n}");
            let _ = commands.try_submit(upsert(&id));
            let _ = commands.try_submit(Command::RemoveSource {
                op: OperationId::new(),
                id,
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        driver_stop.stop();
        (went_live, went_dark)
    });

    let report = pipeline
        .run_until_serving(
            &stop,
            publisher.as_ref(),
            &preview_slot,
            move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
        )
        .await
        .expect("serving run with live network add/remove");
    let (went_live, went_dark) = driver.await.expect("driver task");
    hub.shutdown();

    assert!(
        went_live,
        "the live-added FILE source must reach LIVE via the hub-spawned ingest_loop \
         (observed on real tile.state events)"
    );
    assert!(
        went_dark,
        "the removed source's cell must return to NO_SIGNAL (slate)"
    );
    assert!(
        !report.faltered,
        "live network add/remove churn must never falter the output clock (inv #1 + #10)"
    );
}
