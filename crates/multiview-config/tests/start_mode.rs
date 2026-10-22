//! `[control] start = "boot" | "resume"` — the ADR-W024 cold-start policy
//! token: typed (an unknown token fails parse), defaulting to `boot`, and
//! round-tripping TOML.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, StartMode};

/// A minimal valid document carrying a `[control]` block with `start` spliced
/// in via `{start_line}`.
fn doc(start_line: &str) -> String {
    format!(
        r##"schema_version = 1
[canvas]
width = 64
height = 64
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
[control]
listen = "[::1]:0"
{start_line}
[[outputs]]
kind = "hls"
path = "/tmp/start-mode.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

#[test]
fn start_defaults_to_boot() {
    let config = MultiviewConfig::load_from_toml(&doc("")).expect("parse");
    config.validate().expect("validate");
    let control = config.control.expect("[control] present");
    assert_eq!(
        control.start,
        StartMode::Boot,
        "an absent start token must default to the boot policy"
    );
}

#[test]
fn start_resume_parses() {
    let config = MultiviewConfig::load_from_toml(&doc("start = \"resume\"")).expect("parse");
    config.validate().expect("validate");
    assert_eq!(config.control.expect("[control]").start, StartMode::Resume);
}

#[test]
fn start_boot_parses_explicitly() {
    let config = MultiviewConfig::load_from_toml(&doc("start = \"boot\"")).expect("parse");
    assert_eq!(config.control.expect("[control]").start, StartMode::Boot);
}

#[test]
fn an_unknown_start_token_fails_parse() {
    let err = MultiviewConfig::load_from_toml(&doc("start = \"sometimes\""))
        .expect_err("an unknown start token must be rejected at parse");
    let text = err.to_string();
    assert!(
        text.contains("start") || text.contains("sometimes") || text.contains("variant"),
        "the error should name the bad token/field, got: {text}"
    );
}

#[test]
fn start_round_trips_toml() {
    let config = MultiviewConfig::load_from_toml(&doc("start = \"resume\"")).expect("parse");
    let rendered = config.to_toml().expect("render");
    let reparsed = MultiviewConfig::load_from_toml(&rendered).expect("reparse");
    assert_eq!(
        reparsed.control.expect("[control]").start,
        StartMode::Resume,
        "the resume policy must survive a TOML round-trip"
    );
}
