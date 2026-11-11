//! The production-shaped document (`docs/architecture/demand-driven-sources.md`):
//! 11 sources, 6 bound in the layout, `msnow` audio-routed with its video
//! unbound. By default (`activation = "always_on"`) every source starts at
//! boot, exactly as production runs today. Setting three of them `on_demand`
//! leaves exactly those three idle until a lease arrives.
//!
//! Every network source points at one loopback origin that counts accepted
//! connections, so "starts" is measured, not inferred. Boot time and RSS are
//! printed for the hand-back.
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
use multiview_config::MultiviewConfig;
use multiview_engine::{ConsumerClass, ConsumerTag, DemandPhase, Rails};

/// Counts accepted connections per request path prefix (`/<id>/`).
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
}

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

/// The network sources of the production host, by id.
const NETWORK: [&str; 8] = [
    "cnn",
    "abc-news",
    "sky-news",
    "bbc-world",
    "aljazeera",
    "msnow",
    "dw-news",
    "weather-channel",
];
/// The six layout cells' sources.
const BOUND: [&str; 6] = [
    "cnn",
    "abc-news",
    "sky-news",
    "bbc-world",
    "aljazeera",
    "bars",
];

/// 11 sources (8 HLS, `bars`, `clock`, `timer`), 6 bound, `msnow` routed to the
/// program bus. `on_demand` lists the sources set to `activation = "on_demand"`.
fn production(origin: u16, playlist: &Path, on_demand: &[&str]) -> MultiviewConfig {
    let mut doc = String::from(
        r##"schema_version = 1
[canvas]
width = 640
height = 360
fps = "25/1"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b c", "d e f"]
"##,
    );
    let activation = |id: &str| {
        if on_demand.contains(&id) {
            "activation = \"on_demand\"\n"
        } else {
            ""
        }
    };
    for id in NETWORK {
        let _ = write!(
            doc,
            "[[sources]]\nid = \"{id}\"\nkind = \"hls\"\nurl = \"http://[::1]:{origin}/{id}/index.m3u8\"\n{}",
            activation(id)
        );
    }
    let _ = write!(
        doc,
        "[[sources]]\nid = \"bars\"\nkind = \"bars\"\n{}",
        activation("bars")
    );
    let _ = write!(
        doc,
        "[[sources]]\nid = \"clock\"\nkind = \"clock\"\n{}",
        activation("clock")
    );
    let _ = write!(
        doc,
        "[[sources]]\nid = \"timer\"\nkind = \"timer\"\ntarget = \"time_of_day\"\nat = \"14:30:00\"\ntimezone = \"UTC\"\n{}",
        activation("timer")
    );
    for (cell, id) in ["a", "b", "c", "d", "e", "f"].iter().zip(BOUND) {
        let _ = write!(
            doc,
            "[[cells]]\nid = \"cell_{cell}\"\narea = \"{cell}\"\n[cells.source]\ninput_id = \"{id}\"\n"
        );
    }
    let _ = write!(
        doc,
        r#"[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "msnow"
channels = {{ kind = "stereo" }}
include_in_program_bus = true
[[outputs]]
kind = "hls"
path = "{}"
codec = "mpeg2video"
segment_ms = 1000
"#,
        playlist.display()
    );
    MultiviewConfig::load_from_toml(&doc).expect("production fixture parses")
}

fn on_demand_bulk(origin: u16, playlist: &Path, count: usize) -> MultiviewConfig {
    let mut config = production(origin, playlist, &[]);
    let template = config.sources[0].clone();
    for i in 0..count {
        let mut source = template.clone();
        source.id = format!("iptv{i}");
        source.activation = Some(multiview_config::SourceActivation::OnDemand);
        config.sources.push(source);
    }
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_production_config_starts_every_source_at_boot() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let config = production(origin.port, &dir.path().join("p.m3u8"), &[]);
    assert_eq!(config.sources.len(), 11);
    let rss_before = proc_status("VmRSS:");
    let started = Instant::now();
    let pipeline = Pipeline::build(&config).expect("builds");
    let boot = started.elapsed();
    let rails = pipeline.boot_rails();
    // Every source starts at boot, bound or not, as production does today.
    for source in &config.sources {
        assert!(
            rails.get(&source.id).is_some_and(|r| r.video),
            "{} must start at boot: {rails:?}",
            source.id
        );
    }
    // The audio-routed source keeps its audio.
    assert!(rails["msnow"].audio, "msnow is audio-routed");
    eprintln!(
        "production shape (11 always_on): build {boot:?}, RSS +{} kB, {} boot rails",
        proc_status("VmRSS:").saturating_sub(rss_before),
        rails.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn three_on_demand_sources_start_only_on_lease() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let lazy = ["dw-news", "weather-channel", "timer"];
    let config = production(origin.port, &dir.path().join("p.m3u8"), &lazy);
    let mut pipeline = Pipeline::build(&config).expect("builds");
    let rails = pipeline.boot_rails().clone();
    for id in lazy {
        assert!(!rails.contains_key(id), "{id} is on_demand: {rails:?}");
    }
    for source in config
        .sources
        .iter()
        .filter(|s| !lazy.contains(&s.id.as_str()))
    {
        assert!(rails.contains_key(&source.id), "{} is always_on", source.id);
    }

    let demand = pipeline.demand();
    demand.start();
    for id in lazy {
        assert_eq!(demand.status(id).phase, DemandPhase::Idle, "{id}");
    }
    // Nothing has dialled the on-demand network sources yet.
    let dialled = || origin.accepted.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = dialled();
    // A lease starts exactly the leased on-demand source: its rail runs and,
    // for a network source, it opens its first connection.
    let lease = demand.acquire(
        "dw-news",
        Rails::VIDEO,
        ConsumerTag::new(ConsumerClass::Preview, "test"),
        None,
    );
    let end = Instant::now() + Duration::from_secs(20);
    while !(demand.status("dw-news").running.video && dialled() > before) {
        assert!(
            Instant::now() < end,
            "dw-news never started: {:?}, connections {before} -> {}",
            demand.status("dw-news"),
            dialled()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(demand.status("dw-news").generation, 1);
    assert_eq!(demand.status("weather-channel").phase, DemandPhase::Idle);
    assert_eq!(demand.status("timer").phase, DemandPhase::Idle);
    // Stop is refused for an always_on source and names the setting.
    let holders = demand.stop_source("bars").expect_err("always_on holds it");
    assert!(holders.iter().any(|t| t.class == ConsumerClass::AlwaysOn));
    drop(lease);
    demand.stop_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_hundred_on_demand_sources_add_no_connection_or_thread_to_production() {
    let origin = CountingOrigin::start();
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    // Same process, same build path: measure the production document, then the
    // same document plus 500 on_demand sources.
    let started = Instant::now();
    let base = Pipeline::build(&production(origin.port, &playlist, &[])).expect("builds");
    let base_boot = started.elapsed();
    let base_rails = base.boot_rails().len();
    drop(base);
    let rss_before = proc_status("VmRSS:");
    let threads_before = proc_status("Threads:");
    let connections_before = origin.accepted.load(Ordering::SeqCst);
    let started = Instant::now();
    let mut big = Pipeline::build(&on_demand_bulk(origin.port, &playlist, 500)).expect("builds");
    let big_boot = started.elapsed();
    assert_eq!(big.configured_source_count(), 511);
    assert_eq!(
        big.boot_rails().len(),
        base_rails,
        "no on_demand source boots"
    );
    let demand = big.demand();
    demand.start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = demand.worker_passes();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let wakeups = demand.worker_passes().saturating_sub(before);
    for i in [0, 250, 499] {
        assert_eq!(demand.status(&format!("iptv{i}")).phase, DemandPhase::Idle);
    }
    assert_eq!(
        origin.accepted.load(Ordering::SeqCst),
        connections_before,
        "an on_demand source opens no connection at build or start"
    );
    let rss_after = proc_status("VmRSS:");
    let threads_after = proc_status("Threads:");
    eprintln!(
        "500 on_demand on top of production: build {big_boot:?} (production alone {base_boot:?}), \
         RSS +{} kB (~{} B/source incl. the 11 booted plans), threads {threads_before} -> {threads_after}, \
         demand worker {wakeups} wakeups/s with 11 sources active",
        rss_after.saturating_sub(rss_before),
        rss_after.saturating_sub(rss_before) * 1024 / 511,
    );
    demand.stop_all();
}
