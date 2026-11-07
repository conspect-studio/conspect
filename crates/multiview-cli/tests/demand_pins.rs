//! Standing pins under a real `Pipeline` (`docs/architecture/demand-driven-sources.md`):
//! boot and runtime key a source's standing leases by the owning source, so
//! arming automation keeps an `always_on` pin, two rules can share a
//! replacement, and disarming or deleting a source releases what it held.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt::Write as _;

use multiview_cli::live_sources::SourceSpawn;
use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_engine::{ConsumerClass, ConsumerTag, Rails};

fn hls(id: &str, extra: &str) -> String {
    format!(
        "[[sources]]\nid = \"{id}\"\nkind = \"hls\"\nurl = \"http://[::1]:9/{id}/index.m3u8\"\n{extra}"
    )
}

fn rule(armed: bool) -> String {
    format!(
        "[sources.events.automation]\narmed = {armed}\nrestore_on_end = true\nmax_duration_s = 60\n\
         [sources.events.automation.on_break_start]\naction = \"swap_cell\"\ntarget = \"slate\"\n"
    )
}

fn config(armed: bool) -> MultiviewConfig {
    let mut doc = String::from(
        r##"schema_version = 1
[canvas]
width = 64
height = 64
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
"##,
    );
    doc.push_str(&hls("slate", "activation = \"on_demand\"\n"));
    // `watched` keeps the default always_on activation.
    doc.push_str(&hls("watched", ""));
    doc.push_str(&rule(armed));
    doc.push_str(&hls("other", "activation = \"on_demand\"\n"));
    doc.push_str(&rule(true));
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    let _ = write!(
        doc,
        "[[outputs]]\nkind = \"hls\"\npath = \"{}\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n",
        playlist.display()
    );
    MultiviewConfig::load_from_toml(&doc).expect("fixture parses")
}

fn holders(tags: &[ConsumerTag]) -> Vec<(ConsumerClass, String)> {
    tags.iter()
        .map(|tag| (tag.class, tag.label.to_string()))
        .collect()
}

/// Review DMD-01: an armed `always_on` source keeps its ALL pin, two rules
/// sharing a replacement both hold it, and disarming one releases exactly
/// its own boot pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_pins_are_owned_by_their_source_and_merge() {
    let mut pipeline = Pipeline::build(&config(true)).expect("builds");
    let demand = pipeline.demand();
    let watched = demand.status("watched");
    assert_eq!(
        watched.wanted,
        Rails::ALL,
        "the always_on pin survives arming"
    );
    assert!(holders(&watched.consumers).contains(&(ConsumerClass::AlwaysOn, "always_on".into())));
    assert!(
        holders(&watched.consumers).contains(&(ConsumerClass::EventAutomation, "watched".into()))
    );
    let slate = holders(&demand.status("slate").consumers);
    assert_eq!(
        slate,
        vec![
            (ConsumerClass::EventAutomation, "other".into()),
            (ConsumerClass::EventAutomation, "watched".into()),
        ],
        "both rules hold their shared replacement"
    );
    // Each replacement holder names its owning source as an object, so the
    // control plane can redact it per principal (review DMD-04).
    assert!(demand
        .status("slate")
        .consumers
        .iter()
        .all(|tag| tag.object.as_deref() == Some(&*tag.label)));
    let view = multiview_cli::source_demand::view_of(&demand.status("slate"));
    assert!(view
        .consumers
        .iter()
        .all(|c| c.object_id.as_deref() == Some(c.label.as_str())));

    // Disarm `watched` at runtime: its replacement pin goes, `other`'s stays.
    let spawner = pipeline.live_ingest_spawner();
    let disarmed = config(false)
        .sources
        .into_iter()
        .find(|s| s.id == "watched")
        .unwrap();
    let store = multiview_cli::source_rails::empty_store("watched");
    assert!(spawner.catalogue(&SourceSpawn {
        source: disarmed,
        store
    }));
    let slate = holders(&demand.status("slate").consumers);
    assert_eq!(
        slate,
        vec![(ConsumerClass::EventAutomation, "other".into())]
    );
    let watched = holders(&demand.status("watched").consumers);
    assert_eq!(watched, vec![(ConsumerClass::AlwaysOn, "always_on".into())]);
    demand.stop_all();
}

fn bars_doc(armed: bool) -> MultiviewConfig {
    let mut doc = config(true);
    let mut source = doc
        .sources
        .iter()
        .find(|s| s.id == "watched")
        .unwrap()
        .clone();
    source.id = "cam".into();
    source.kind = multiview_config::SourceKind::Bars;
    if let Some(rule) = source
        .events
        .as_mut()
        .and_then(|events| events.automation.as_mut())
    {
        rule.armed = armed;
    }
    doc.sources.push(source);
    doc
}

/// Review DMD-09: arming or disarming automation through the live-source hub
/// changes pins only; the running producer is not restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arming_or_disarming_automation_never_restarts_the_producer() {
    let doc = bars_doc(false);
    let mut pipeline = Pipeline::build(&doc).expect("builds");
    let demand = pipeline.demand();
    demand.start();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !demand.status("cam").running.video {
        assert!(
            std::time::Instant::now() < end,
            "{:?}",
            demand.status("cam")
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let generation = demand.status("cam").generation;
    let transitions = demand.transitions();
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
        Some(pipeline.live_ingest_spawner()),
    );
    let handle = hub.handle();
    for armed in [true, false, true] {
        let source = bars_doc(armed)
            .sources
            .into_iter()
            .find(|s| s.id == "cam")
            .unwrap();
        let store = pipeline.catalogue().load()["cam"].store().clone();
        assert_eq!(
            handle.request_spawn_source(SourceSpawn { source, store }),
            multiview_cli::live_sources::HubSubmit::Accepted
        );
        let want = usize::from(armed);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while demand
            .status("slate")
            .consumers
            .iter()
            .filter(|tag| &*tag.label == "cam")
            .count()
            != want
        {
            assert!(
                std::time::Instant::now() < end,
                "pins never followed armed={armed}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        demand.status("cam").generation,
        generation,
        "producer restarted"
    );
    // Only the on-demand replacement (`slate`) may have started or stopped.
    let moved = demand.transitions() - transitions;
    assert!(moved <= 3, "unexpected transitions: {moved}");
    assert!(demand.status("cam").running.video, "still running");
    hub.shutdown();
    demand.stop_all();
}

/// Review R2-02: a running source with NO events block gains its first
/// automation rule (armed, then disarmed, then removed): pins follow, the
/// producer never restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_rule_on_a_source_without_events_never_restarts_the_producer() {
    let mut doc = config(false);
    let mut bare = doc
        .sources
        .iter()
        .find(|s| s.id == "watched")
        .unwrap()
        .clone();
    bare.id = "cam".into();
    bare.kind = multiview_config::SourceKind::Bars;
    let rule = bare
        .events
        .take()
        .and_then(|events| events.automation)
        .unwrap();
    let ruled = |armed: bool| {
        let mut source = bare.clone();
        let mut rule = rule.clone();
        rule.armed = armed;
        source.events = Some(multiview_config::schema::SourceEventsConfig {
            observe: true,
            automation: Some(rule),
        });
        source
    };
    doc.sources.push(bare.clone());
    let mut pipeline = Pipeline::build(&doc).expect("builds");
    let demand = pipeline.demand();
    demand.start();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !demand.status("cam").running.video {
        assert!(
            std::time::Instant::now() < end,
            "{:?}",
            demand.status("cam")
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let generation = demand.status("cam").generation;
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
        Some(pipeline.live_ingest_spawner()),
    );
    let handle = hub.handle();
    for (armed, want) in [
        (Some(true), 1_usize),
        (Some(false), 0),
        (Some(true), 1),
        (None, 0),
    ] {
        let source = match armed {
            Some(armed) => ruled(armed),
            None => bare.clone(),
        };
        let store = pipeline.catalogue().load()["cam"].store().clone();
        assert_eq!(
            handle.request_spawn_source(SourceSpawn { source, store }),
            multiview_cli::live_sources::HubSubmit::Accepted
        );
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while demand
            .status("slate")
            .consumers
            .iter()
            .filter(|tag| &*tag.label == "cam")
            .count()
            != want
        {
            assert!(
                std::time::Instant::now() < end,
                "pins never followed armed={armed:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        demand.status("cam").generation,
        generation,
        "adding the first rule restarted the producer"
    );
    assert!(demand.status("cam").running.video, "still running");
    hub.shutdown();
    demand.stop_all();
}

/// Review DMD-05: deleting a started source through the hub releases its
/// operator Start, and re-adding the id does not inherit it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_started_source_releases_its_start() {
    let mut pipeline = Pipeline::build(&config(false)).expect("builds");
    let demand = pipeline.demand();
    let spawner = pipeline.live_ingest_spawner();
    let slate = config(false)
        .sources
        .into_iter()
        .find(|s| s.id == "slate")
        .unwrap();
    for _ in 0..3 {
        let store = multiview_cli::source_rails::empty_store("slate");
        assert!(spawner.catalogue(&SourceSpawn {
            source: slate.clone(),
            store
        }));
        demand.start_source("slate");
        assert!(holders(&demand.status("slate").consumers)
            .contains(&(ConsumerClass::Manual, "operator".into())));
        spawner.source_removed("slate");
        assert!(
            !holders(&demand.status("slate").consumers)
                .iter()
                .any(|(class, _)| *class == ConsumerClass::Manual),
            "deleted source keeps its Start"
        );
    }
    demand.stop_all();
}
