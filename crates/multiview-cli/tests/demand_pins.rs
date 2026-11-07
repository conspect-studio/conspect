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
