//! Demand-driven sources (`docs/architecture/demand-driven-sources.md`): every
//! source starts at boot by default (`activation = "always_on"`), exactly as
//! before; a source set `on_demand` costs nothing until a consumer needs it.
//!
//! Every network source here points at one loopback listener that counts the
//! connections it accepts, so "zero outbound connections" is measured, not
//! inferred. Thread counts and RSS come from `/proc/self/status`.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt::Write as _;
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::pipeline::Pipeline;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_engine::Rails;

/// A loopback listener that counts the connections it accepts.
struct CountingOrigin {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

impl CountingOrigin {
    fn start() -> Self {
        let listener = TcpListener::bind("[::1]:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_ok() {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        Self { port, accepted }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// A `/proc/self/status` field in its unit (kB for memory, a count for threads).
fn proc_status(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix(field)
                    .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
            })
        })
        .unwrap_or(0)
}

/// Thread names of this process, for asserting which per-source threads exist.
fn thread_names() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
                .map(|name| name.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// One bound `bars` source plus `idle` `on_demand` HLS sources (half with
/// captions), all on `origin`.
fn config(origin_port: u16, idle: usize, playlist: &Path, extra: &str) -> MultiviewConfig {
    let mut doc = String::from(
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
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "bound"
kind = "bars"
"##,
    );
    for i in 0..idle {
        let _ = write!(
            doc,
            "[[sources]]\nid = \"idle{i}\"\nkind = \"hls\"\nurl = \"http://[::1]:{origin_port}/idle{i}/index.m3u8\"\nactivation = \"on_demand\"\n"
        );
        if i % 2 == 0 {
            doc.push_str("captions = { mode = \"auto\" }\n");
        }
    }
    doc.push_str(extra);
    let _ = write!(
        doc,
        r#"[[cells]]
id = "cell"
area = "a"
[cells.source]
input_id = "bound"
[[outputs]]
kind = "hls"
path = "{}"
codec = "mpeg2video"
segment_ms = 1000
"#,
        playlist.display()
    );
    MultiviewConfig::load_from_toml(&doc).expect("fixture parses")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_hundred_on_demand_sources_cost_no_connection_probe_or_thread() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");

    // Baseline: the same document and run with no idle sources, measured the
    // same way, so the difference is what 500 idle sources cost.
    let small = config(origin.port, 0, &playlist, "");
    let started = Instant::now();
    let mut baseline = Pipeline::build(&small).expect("baseline builds");
    let small_build = started.elapsed();
    baseline.run_for(10).await.expect("baseline run");
    std::thread::sleep(Duration::from_millis(300));
    let threads_small = proc_status("Threads:");
    let rss_small = proc_status("VmRSS:");
    drop(baseline);

    let large = config(origin.port, 500, &playlist, "");
    let started = Instant::now();
    let mut pipeline = Pipeline::build(&large).expect("500-source document builds");
    let build_time = started.elapsed();
    assert_eq!(pipeline.configured_source_count(), 501);
    assert_eq!(
        pipeline.boot_rails().keys().collect::<Vec<_>>(),
        vec!["bound"],
        "only the bound source starts at boot"
    );

    // Run so any eager connect, probe or caption fetch would land.
    let report = pipeline.run_for(10).await.expect("bounded run");
    assert_eq!(report.frames, 10);
    std::thread::sleep(Duration::from_millis(300));
    let threads_large = proc_status("Threads:");
    let rss_large = proc_status("VmRSS:");
    let (threads_before, threads_after, rss_before, rss_after) =
        (threads_small, threads_large, rss_small, rss_large);

    assert_eq!(
        origin.accepted(),
        0,
        "an unconsumed source must open no connection (probe, caption fetch or decode)"
    );
    let idle_threads: Vec<String> = thread_names()
        .into_iter()
        // Per-source producers are named `multiview-<role>-<id>` (comm is
        // truncated to 15 bytes, so match the role prefixes).
        .filter(|name| {
            ["multiview-inges", "multiview-audio", "multiview-capti", "multiview-synth", "multiview-yt-re"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .collect();
    // Exactly the bound `bars` generator ran; nothing for any idle source.
    assert!(
        idle_threads
            .iter()
            .all(|name| name.starts_with("multiview-synth")),
        "idle sources started producer threads: {idle_threads:?}"
    );

    // Reported, not asserted: hosts differ. The design note quotes a run.
    eprintln!(
        "demand idle-cost: 500 idle sources -> build {build_time:?} (0 idle: {small_build:?}), \
         threads after a run {threads_after} (0 idle: {threads_before}), RSS after a run \
         {rss_after} kB (0 idle: {rss_before} kB)"
    );
    assert!(
        threads_after <= threads_before + 2,
        "500 idle sources added threads: {threads_before} -> {threads_after}"
    );
    assert!(
        build_time < Duration::from_secs(5),
        "building 500 idle sources took {build_time:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routed_unbound_source_runs_audio_only_with_no_video_decode() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    let extra = r#"[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "idle3"
channels = { kind = "stereo" }
include_in_program_bus = true
"#;
    let config = config(origin.port, 5, &playlist, extra);
    let pipeline = Pipeline::build(&config).expect("builds");
    assert_eq!(
        pipeline.boot_rails().get("idle3"),
        Some(&Rails::AUDIO),
        "msnow shape: routed audio, unbound video"
    );
    assert!(!pipeline.boot_rails().contains_key("idle4"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_with_no_activation_starts_at_boot_and_the_legacy_alias_loads() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    let extra = format!(
        "[[sources]]\nid = \"plain\"\nkind = \"hls\"\nurl = \"http://[::1]:{0}/a.m3u8\"\n\
         [[sources]]\nid = \"pinned\"\nkind = \"hls\"\nurl = \"http://[::1]:{0}/p.m3u8\"\ndemand = {{ always_on = true }}\n\
         [[sources]]\nid = \"lazy\"\nkind = \"hls\"\nurl = \"http://[::1]:{0}/l.m3u8\"\ndemand = {{ always_on = false }}\n",
        origin.port
    );
    let config = config(origin.port, 2, &playlist, &extra);
    let pipeline = Pipeline::build(&config).expect("builds");
    assert_eq!(pipeline.boot_rails().get("plain"), Some(&Rails::ALL));
    assert_eq!(pipeline.boot_rails().get("pinned"), Some(&Rails::ALL));
    assert!(!pipeline.boot_rails().contains_key("lazy"));
}

#[test]
fn the_idle_cost_of_500_sources_is_a_catalogue_entry_each() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    let large = config(origin.port, 500, &playlist, "");
    // Warm the allocator and every lazy static with one throwaway build.
    drop(Pipeline::build(&config(origin.port, 0, &playlist, "")).expect("warm-up"));
    let before = proc_status("VmRSS:");
    let pipelines: Vec<Pipeline> = (0..4)
        .map(|_| Pipeline::build(&large).expect("builds"))
        .collect();
    let after = proc_status("VmRSS:");
    let per_build_kb = after.saturating_sub(before) / 4;
    eprintln!(
        "demand idle-cost: one 500-idle-source pipeline adds ~{per_build_kb} kB RSS \
         (~{} bytes per idle source), {} connections",
        per_build_kb * 1024 / 500,
        origin.accepted()
    );
    assert_eq!(origin.accepted(), 0);
    // O(config size): far below one decoder's frame buffers (a 1080p NV12
    // frame alone is ~3 MB).
    assert!(
        per_build_kb < 16 * 1024,
        "{per_build_kb} kB per 500-source build"
    );
    drop(pipelines);
}

/// Idle sources raise no alarm: the only tile-state and fault surfaces are the
/// cells the output composites, so an unconsumed source never reaches them. A
/// whole run with 50 idle sources (pointed at a dead origin) emits no event
/// naming any of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_sources_raise_no_tile_state_or_alarm_events() {
    use multiview_control::EngineStateSnapshot;
    use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
    use multiview_events::Event;

    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    let config = config(origin.port, 50, &playlist, "");
    let mut pipeline = Pipeline::build(&config).expect("builds");
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(4096));
    let mut events = publisher.subscribe();
    let slot = multiview_cli::preview::program_slot();
    let stop = StopSignal::new();
    let stopper = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stopper.stop();
    });
    Box::pin(pipeline.run_until_serving(
        &stop,
        publisher.as_ref(),
        &slot,
        |_drive: &mut CompositorDrive<Nv12Image>| {},
    ))
    .await
    .expect("run");

    let mut named_idle = Vec::new();
    while let Ok(seq) = events.try_recv() {
        let event = seq.event.as_ref();
        let text = format!("{event:?}");
        let idle = (0..50).any(|i| text.contains(&format!("\"idle{i}\"")));
        let alarm_or_tile = matches!(
            event,
            Event::TileState(_) | Event::AlarmRaised(_) | Event::AlarmUpdated(_)
        );
        if idle && alarm_or_tile {
            named_idle.push(text);
        }
    }
    assert!(named_idle.is_empty(), "idle sources raised: {named_idle:?}");
    assert_eq!(origin.accepted(), 0);
}
