//! Lossless passthrough of an ADR-M012 `[switcher]` block before SW-17 lands the
//! typed domain.
//!
//! The switcher domain (mix/effects stages, transitions, keyers, …) lands with
//! SW-17, but a document authored against the ADR-M012 schema must already
//! **parse and round-trip byte-stably** rather than being silently emptied: a
//! `SwitcherConfig` that drops every unmodelled field emits an empty
//! `[switcher]` block (round-trip broken) and makes [`ConfigDiff`] blind to
//! edits inside the block. These tests pin the passthrough (`serde(flatten)`)
//! and the restart-class diff signal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ConfigDiff, MultiviewConfig};

/// A complete, valid single-source document; each test appends a `[switcher]`
/// block to it.
const BASE: &str = r##"
schema_version = 1

[canvas]
width = 640
height = 360
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
id = "in_a"
kind = "test"

[[sources]]
id = "in_b"
kind = "bars"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/switcher-passthrough/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// A populated ADR-M012 switcher block: nested **arrays of tables** and a
/// scalar mix, exactly the shape the typed schema will model in SW-17.
const POPULATED_SWITCHER: &str = r#"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
flip_flop = true

[[switcher.mix_effects.upstream_keys]]
id = "uk1"
kind = "linear"
fill_source = "in_a"

[[switcher.mix_effects.upstream_keys]]
id = "uk2"
kind = "linear"
fill_source = "in_b"

[[switcher.transitions]]
id = "house-mix"
kind = "mix"
rate_frames = 25
"#;

/// The same block with one `me1` field changed — the edit [`ConfigDiff`] must
/// see.
const CHANGED_SWITCHER: &str = r#"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "aux"
flip_flop = true

[[switcher.mix_effects.upstream_keys]]
id = "uk1"
kind = "linear"
fill_source = "in_a"

[[switcher.mix_effects.upstream_keys]]
id = "uk2"
kind = "linear"
fill_source = "in_b"

[[switcher.transitions]]
id = "house-mix"
kind = "mix"
rate_frames = 25
"#;

fn parse(doc: &str) -> MultiviewConfig {
    let cfg = MultiviewConfig::load_from_toml(doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

#[test]
fn a_populated_switcher_block_round_trips_byte_stably_through_toml() {
    let cfg = parse(&format!("{BASE}{POPULATED_SWITCHER}"));

    let once = cfg.to_toml().expect("serialize to TOML");

    // The unmodelled fields are carried, not emptied: the nested arrays of
    // tables and their values must appear in the serialized document.
    assert!(
        once.contains("[[switcher.mix_effects]]"),
        "the mix/effects table must survive serialization: {once}"
    );
    assert!(
        once.contains("id = \"me1\""),
        "the me1 id must survive serialization: {once}"
    );
    assert!(
        once.contains("[[switcher.mix_effects.upstream_keys]]"),
        "the nested upstream-key tables must survive serialization: {once}"
    );
    assert!(
        once.contains("[[switcher.transitions]]"),
        "the sibling transitions table must survive serialization: {once}"
    );
    assert!(
        once.contains("rate_frames = 25"),
        "integer fields must survive serialization: {once}"
    );

    // Lossless and byte-stable: re-parsing the emitted document yields an equal
    // document, and re-serializing it is byte-identical (idempotent).
    let from_toml = MultiviewConfig::load_from_toml(&once).expect("re-parse TOML");
    assert_eq!(cfg, from_toml, "TOML round-trip must be lossless");
    assert_eq!(
        from_toml.to_toml().expect("re-serialize to TOML"),
        once,
        "TOML serialization must be byte-stable"
    );
}

#[test]
fn a_populated_switcher_block_round_trips_byte_stably_through_json() {
    let cfg = parse(&format!("{BASE}{POPULATED_SWITCHER}"));

    let once = cfg.to_json().expect("serialize to JSON");

    assert!(
        once.contains("\"mix_effects\""),
        "the mix/effects array must survive serialization: {once}"
    );
    assert!(
        once.contains("\"me1\""),
        "the me1 id must survive serialization: {once}"
    );
    assert!(
        once.contains("\"upstream_keys\""),
        "the nested upstream-key array must survive serialization: {once}"
    );

    let from_json = MultiviewConfig::load_from_json(&once).expect("re-parse JSON");
    assert_eq!(cfg, from_json, "JSON round-trip must be lossless");
    assert_eq!(
        from_json.to_json().expect("re-serialize to JSON"),
        once,
        "JSON serialization must be byte-stable"
    );
}

#[test]
fn a_changed_switcher_field_is_a_restart_class_change() {
    let running = parse(&format!("{BASE}{POPULATED_SWITCHER}"));
    let next = parse(&format!("{BASE}{CHANGED_SWITCHER}"));

    let diff = ConfigDiff::between(&running, &next);

    assert!(
        diff.changed_sections.contains("switcher"),
        "a changed field inside `[switcher]` must surface the restart-class \
         `switcher` section, not vanish: {diff:?}"
    );
    assert!(!diff.is_empty(), "a real change must never diff empty");
}

#[test]
fn an_unchanged_switcher_block_diffs_empty() {
    let running = parse(&format!("{BASE}{POPULATED_SWITCHER}"));
    let next = parse(&format!("{BASE}{POPULATED_SWITCHER}"));

    let diff = ConfigDiff::between(&running, &next);

    assert!(
        diff.is_empty(),
        "identical populated switcher blocks must diff empty: {diff:?}"
    );
}
