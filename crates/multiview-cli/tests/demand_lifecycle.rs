//! Demand lifecycle in a real run (`docs/architecture/demand-driven-sources.md`):
//! a cell re-pointed to an idle source starts it off-clock (NO_SIGNAL until its
//! first frame), re-pointing away stops it after the linger, a shared source
//! never restarts across a swap, and lease churn during live output never
//! slips the output clock.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::control;
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::program_slot;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::routing::StreamRef;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_core::stream::StreamKind;
use multiview_engine::{
    CompositorDrive, ConsumerClass, ConsumerTag, DemandPhase, EnginePublisher, Rails, StopSignal,
};
use multiview_events::Event;

fn config(playlist: &Path, linger_secs: u32) -> MultiviewConfig {
    let doc = format!(
        r##"schema_version = 1
[canvas]
width = 320
height = 180
fps = "25/1"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "shared"
kind = "solid"
color = "#404040"
[[sources]]
id = "dark"
kind = "solid"
color = "#101010"
[[sources]]
id = "bright"
kind = "solid"
color = "#f0f0f0"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "shared"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "dark"
[system.demand]
linger_secs = {linger_secs}
[[outputs]]
kind = "hls"
path = "{}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist.display()
    );
    let config = MultiviewConfig::load_from_toml(&doc).expect("fixture parses");
    config.validate().expect("fixture validates");
    config
}

fn route(cell: &str, source: &str) -> Command {
    Command::RouteVideo {
        op: OperationId::new(),
        cell: cell.to_owned(),
        source: StreamRef::best(source, StreamKind::Video),
    }
}

async fn eventually(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Re-point cell_b dark -> bright -> dark. `bright` starts on demand, `dark`
/// lingers then stops, `shared` (bound throughout) never restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_route_starts_an_idle_source_and_routing_away_stops_it_after_the_linger() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir.path().join("p.m3u8"), 1);
    let mut pipeline = Pipeline::build(&config).expect("builds");
    assert!(
        !pipeline.boot_rails().contains_key("bright"),
        "bright is idle at boot"
    );
    let demand = pipeline.demand();

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let slot = program_slot();
    let (commands, command_rx) = command_bus(16);
    let stop = StopSignal::new();
    let hub = multiview_cli::live_sources::LiveSourceHub::start(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
    );
    let mut drain = control::command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
    );

    let watcher = demand.clone();
    let relay_stop = stop.clone();
    let relay = tokio::spawn(async move {
        eventually("shared is live", Duration::from_secs(10), || {
            watcher.status("shared").phase == DemandPhase::Live
        })
        .await;
        let shared_transitions = watcher.transitions();
        assert_eq!(watcher.status("bright").phase, DemandPhase::Idle);

        commands.try_submit(route("cell_b", "bright")).unwrap();
        eventually("bright starts on demand", Duration::from_secs(10), || {
            watcher.status("bright").phase == DemandPhase::Live
        })
        .await;
        let bright = watcher.status("bright");
        assert!(bright
            .consumers
            .iter()
            .any(|c| c.class == ConsumerClass::LayoutCell));

        // dark lost its only consumer: it lingers, then stops.
        let dark = watcher.status("dark");
        assert!(dark.consumers.is_empty(), "dark has no consumer: {dark:?}");
        assert!(dark.running.video, "dark lingers");
        eventually(
            "dark stops after the linger",
            Duration::from_secs(10),
            || watcher.status("dark").phase == DemandPhase::Idle,
        )
        .await;

        commands.try_submit(route("cell_b", "dark")).unwrap();
        eventually("dark restarts", Duration::from_secs(10), || {
            watcher.status("dark").phase == DemandPhase::Live
        })
        .await;
        let shared = watcher.status("shared");
        assert_eq!(shared.phase, DemandPhase::Live, "shared never stopped");
        assert!(watcher.transitions() > shared_transitions);
        relay_stop.stop();
    });

    let report = Box::pin(pipeline.run_until_serving(
        &stop,
        publisher.as_ref(),
        &slot,
        move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
    ))
    .await
    .expect("serving run");
    relay.await.expect("relay");
    assert!(
        !report.faltered,
        "demand changes must not falter the output"
    );
    hub.shutdown();
}

/// Chaos: hammer lease churn from several threads while the output runs. The
/// clock must keep exactly one frame per tick and never falter, and nothing
/// may be left running afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn lease_churn_during_live_output_never_slips_the_clock() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir.path().join("p.m3u8"), 0);
    let mut pipeline = Pipeline::build(&config).expect("builds");
    let demand = pipeline.demand();

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let slot = program_slot();
    let stop = StopSignal::new();

    let churners: Vec<_> = (0..4)
        .map(|t| {
            let demand = demand.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i = 0_u64;
                while !stop.is_stopped() {
                    let id =
                        ["bright", "dark", "shared", "missing"][usize::try_from(i % 4).unwrap()];
                    let tag = ConsumerTag::new(ConsumerClass::Preview, format!("churn{t}"));
                    let lease = demand.acquire(id, Rails::ALL, tag, None);
                    if i % 3 == 0 {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    drop(lease);
                    i += 1;
                }
                i
            })
        })
        .collect();

    let stopper = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        stopper.stop();
    });
    let started = Instant::now();
    let report = Box::pin(pipeline.run_until_serving(
        &stop,
        publisher.as_ref(),
        &slot,
        |_drive: &mut CompositorDrive<Nv12Image>| {},
    ))
    .await
    .expect("serving run");
    let elapsed = started.elapsed();
    let churned: u64 = churners.into_iter().map(|t| t.join().unwrap()).sum();

    assert!(churned > 100, "the churn actually ran ({churned})");
    assert!(
        !report.faltered,
        "lease churn must never shed output frames"
    );
    // 25 fps: the frame count tracks wall time, never the churn.
    let expected = elapsed.as_millis() / 40;
    let frames = u128::from(report.frames);
    assert!(
        frames + 15 >= expected && frames <= expected + 15,
        "{frames} frames in {elapsed:?} (expected ~{expected})"
    );
    eventually("nothing left wanted", Duration::from_secs(5), || {
        demand.snapshot().iter().all(|s| s.consumers.is_empty())
    })
    .await;
}
