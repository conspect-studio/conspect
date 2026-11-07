//! Per-source `events` config field (ADR-0183): explicit opt-in event
//! observation, defaulting ON for HLS, additive (existing configs parse
//! unchanged), and surviving TOML + JSON round-trips.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::schema::SourceEventsConfig;
use multiview_config::MultiviewConfig;

/// A structurally-complete one-source grid document; `events_line` is spliced
/// into the single source block (empty string = no `events` field at all).
fn doc_with_events(events_line: &str) -> String {
    format!(
        "schema_version = 1\n\
         [canvas]\n\
         width = 1280\n\
         height = 720\n\
         fps = \"25/1\"\n\
         pixel_format = \"nv12\"\n\
         background = \"#101014\"\n\
         [canvas.color]\n\
         profile = \"sdr-bt709-limited\"\n\
         [layout]\n\
         kind = \"grid\"\n\
         columns = [\"1fr\"]\n\
         rows = [\"1fr\"]\n\
         gap = 0\n\
         areas = [\"c0\"]\n\
         [[sources]]\n\
         id = \"in_0\"\n\
         kind = \"hls\"\n\
         url = \"https://example.com/index.m3u8\"\n\
         {events_line}\n\
         [[cells]]\n\
         id = \"cell_0\"\n\
         area = \"c0\"\n\
         fit = \"contain\"\n\
         [cells.source]\n\
         input_id = \"in_0\"\n\
         [[outputs]]\n\
         kind = \"rtsp_server\"\n\
         mount = \"/multiview\"\n\
         codec = \"h264\"\n"
    )
}

#[test]
fn events_absent_is_none_and_existing_configs_are_unchanged() {
    let cfg = MultiviewConfig::load_from_toml(&doc_with_events("")).expect("parse");
    assert_eq!(cfg.sources[0].events, None);
    assert!(cfg.validate().is_ok());
}

#[test]
fn events_observe_true_parses_and_round_trips_through_toml_and_json() {
    let cfg = MultiviewConfig::load_from_toml(&doc_with_events("events = { observe = true }"))
        .expect("parse");
    assert_eq!(
        cfg.sources[0].events,
        Some(SourceEventsConfig { observe: true })
    );
    let json = cfg.to_json().expect("to_json");
    assert_eq!(
        MultiviewConfig::load_from_json(&json).expect("from_json"),
        cfg
    );
    let toml_text = cfg.to_toml().expect("to_toml");
    assert_eq!(
        MultiviewConfig::load_from_toml(&toml_text).expect("from_toml"),
        cfg
    );
}

#[test]
fn events_observe_false_parses_to_its_variant() {
    let cfg = MultiviewConfig::load_from_toml(&doc_with_events("events = { observe = false }"))
        .expect("parse");
    assert_eq!(
        cfg.sources[0].events,
        Some(SourceEventsConfig { observe: false })
    );
}

#[test]
fn events_table_present_without_observe_defaults_true() {
    // `events = {}` -> `observe` defaults true (serde default AND the explicit
    // `SourceEventsConfig::default()`), matching the "default ON for HLS" stance
    // documented on the field.
    let cfg = MultiviewConfig::load_from_toml(&doc_with_events("events = {}")).expect("parse");
    assert_eq!(
        cfg.sources[0].events,
        Some(SourceEventsConfig { observe: true })
    );
    assert_eq!(
        SourceEventsConfig::default(),
        SourceEventsConfig { observe: true }
    );
}
