//! SW-13a: config-declared program audio (ADR-0059 §8).
//!
//! Program audio is **declared in config**: the presence of the ADR-R005
//! `[audio]` routing block, or of a `[switcher]` block (a switcher without
//! program audio is not a switcher), makes it default-on;
//! `multiview run --program-audio` remains a legacy override. This file pins the
//! declarative half — the predicate and lossless TOML↔JSON round-trip of the
//! presence-only `[switcher]` root — with no engine and no network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::MultiviewConfig;

/// A complete, valid single-source document with **neither** declaring block;
/// each test appends the block whose presence it exercises.
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

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/program-audio-declared/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// The `[audio]` routing block (ADR-R005) — presence alone declares program
/// audio, whether or not it names any route.
const AUDIO_BLOCK: &str = "[audio]\nsample_rate_hz = 48000\n";

/// The `[switcher]` root (ADR-M012) — presence alone declares program audio.
const SWITCHER_BLOCK: &str = "[switcher]\n";

fn parse(doc: &str) -> MultiviewConfig {
    let cfg = MultiviewConfig::load_from_toml(doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

#[test]
fn a_document_declaring_neither_block_does_not_declare_program_audio() {
    let cfg = parse(BASE);
    assert!(
        !cfg.declares_program_audio(),
        "a document with neither an `[audio]` nor a `[switcher]` block declares no \
         program audio (the absent ⇒ off default the byte-identical guarantee rides)"
    );
}

#[test]
fn an_audio_block_declares_program_audio() {
    let cfg = parse(&format!("{BASE}{AUDIO_BLOCK}"));
    assert!(
        cfg.declares_program_audio(),
        "an `[audio]` routing block declares program audio (ADR-0059 §8)"
    );
}

#[test]
fn a_switcher_block_declares_program_audio() {
    let cfg = parse(&format!("{BASE}{SWITCHER_BLOCK}"));
    assert!(
        cfg.declares_program_audio(),
        "a `[switcher]` block declares program audio by default — a switcher \
         without program audio is not a switcher (ADR-0059 §8)"
    );
}

#[test]
fn a_switcher_block_round_trips_through_toml_and_json_losslessly() {
    let cfg = parse(&format!("{BASE}{SWITCHER_BLOCK}"));

    let toml_text = cfg.to_toml().expect("serialize to TOML");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("re-parse TOML");
    assert_eq!(cfg, from_toml, "TOML round-trip must be lossless");

    let json_text = cfg.to_json().expect("serialize to JSON");
    let from_json = MultiviewConfig::load_from_json(&json_text).expect("re-parse JSON");
    assert_eq!(cfg, from_json, "JSON round-trip must be lossless");

    // The block is actually carried, not silently dropped: the canonical JSON
    // wire form names it, so a config round-trip preserves the declaration.
    assert!(
        json_text.contains("\"switcher\""),
        "the serialized JSON must carry the `switcher` root: {json_text}"
    );
}

#[test]
fn an_audio_block_round_trips_through_toml_and_json_losslessly() {
    let cfg = parse(&format!("{BASE}{AUDIO_BLOCK}"));

    let toml_text = cfg.to_toml().expect("serialize to TOML");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("re-parse TOML");
    assert_eq!(cfg, from_toml, "TOML round-trip must be lossless");

    let json_text = cfg.to_json().expect("serialize to JSON");
    let from_json = MultiviewConfig::load_from_json(&json_text).expect("re-parse JSON");
    assert_eq!(cfg, from_json, "JSON round-trip must be lossless");
}
