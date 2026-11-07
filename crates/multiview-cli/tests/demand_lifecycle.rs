//! Demand lifecycle in a real run (`docs/architecture/demand-driven-sources.md`):
//! a cell re-pointed to an idle source starts it off-clock (`NO_SIGNAL` until its
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

/// Ends a run when dropped, so a failed assertion never leaves it serving.
struct StopOnDrop(StopSignal);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// The run installs its demand seam process-wide (one run per process), so
/// these tests take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Re-point `cell_b` dark -> bright -> dark. `bright` starts on demand, `dark`
/// lingers then stops, `shared` (bound throughout) never restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_route_starts_an_idle_source_and_routing_away_stops_it_after_the_linger() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir.path().join("p.m3u8"), 3);
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
    let mut drain = control::command_drain_with_live_sources_and_demand(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        hub.handle(),
        pipeline.drain_demand(),
    );

    let watcher = demand.clone();
    let relay_stop = stop.clone();
    let relay = tokio::spawn(async move {
        let _guard = StopOnDrop(relay_stop.clone());
        eventually("shared is live", Duration::from_secs(30), || {
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
        assert!(
            dark.running.video,
            "dark lingers (3 s), it does not stop at once"
        );
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

/// One 4 s serving run with `workers` threads churning leases. Returns the
/// frames emitted, whether the output faltered, and the cycles churned.
async fn churn_run(workers: usize) -> (u64, u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir.path().join("p.m3u8"), 0);
    let mut pipeline = Pipeline::build(&config).expect("builds");
    let demand = pipeline.demand();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let slot = program_slot();
    let stop = StopSignal::new();
    let churners: Vec<_> = (0..workers)
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
    let report = Box::pin(pipeline.run_until_serving(
        &stop,
        publisher.as_ref(),
        &slot,
        |_drive: &mut CompositorDrive<Nv12Image>| {},
    ))
    .await
    .expect("serving run");
    let churned: u64 = churners.into_iter().map(|t| t.join().unwrap()).sum();
    eventually("nothing left wanted", Duration::from_secs(5), || {
        demand.snapshot().iter().all(|s| s.consumers.is_empty())
    })
    .await;
    (report.frames, report.dropped, churned)
}

/// Chaos: hammer lease churn (starts, lingers of zero, stops, an unknown id)
/// from four threads while the output runs. The output clock must emit as many
/// frames as the same run with no churn and never falter, and nothing may be
/// left wanted afterwards. Comparative, so a loaded host cannot fake a pass or
/// a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn lease_churn_during_live_output_never_slips_the_clock() {
    let _serial = SERIAL.lock().await;
    let (quiet_frames, quiet_dropped, _) = churn_run(0).await;
    let (frames, dropped, churned) = churn_run(4).await;
    eprintln!(
        "lease churn chaos: {churned} acquire/release cycles; {frames} frames ({dropped} shed \
         by the encoder) vs {quiet_frames} ({quiet_dropped} shed) with no churn, same 4 s run"
    );
    assert!(churned > 1_000, "the churn actually ran ({churned})");
    // The clock emits one frame per tick against wall time: churn must not
    // cost ticks. Encoder sheds are the egress's own overload policy; on a
    // loaded host the quiet run sheds too, so they are compared, not zeroed.
    assert!(
        frames * 10 >= quiet_frames * 9,
        "churn slowed the output clock: {frames} frames vs {quiet_frames}"
    );
    assert!(
        dropped <= quiet_dropped + frames / 10,
        "churn made the encoder shed: {dropped} vs {quiet_dropped}"
    );
}
