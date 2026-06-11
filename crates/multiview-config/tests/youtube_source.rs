//! The `youtube` source kind (ADR-0015): a watch/live URL the resolver turns into
//! an HLS master. This test pins that the kind parses, validates, and round-trips
//! losslessly through the real public config API (parse → validate → re-serialize)
//! — no tautologies, no bare-enum shortcuts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, SourceKind};

/// Wrap one source's `kind` fields in a minimal, structurally valid 1x1 grid
/// document so the real parse + validate path is exercised, not a bare enum.
fn config_with_source(source_kind_fields: &str) -> String {
    format!(
        r##"schema_version = 1
[canvas]
width = 320
height = 240
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
{source_kind_fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

#[test]
fn youtube_source_round_trips_with_its_url() {
    let url = "https://www.youtube.com/watch?v=abcdEFGH123";
    let doc = config_with_source(&format!("kind = \"youtube\"\nurl = \"{url}\""));
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("a youtube source validates");

    match &cfg.sources[0].kind {
        SourceKind::Youtube { url: parsed } => assert_eq!(parsed, url),
        other => panic!("expected Youtube, got {other:?}"),
    }

    // Canonical re-serialization keeps the snake_case tag and the URL.
    let toml = cfg.to_toml().expect("to_toml");
    assert!(
        toml.contains("kind = \"youtube\""),
        "canonical kind tag is `youtube`, got:\n{toml}"
    );
    assert!(
        toml.contains(url),
        "the watch URL must round-trip, got:\n{toml}"
    );
}

#[test]
fn youtube_kind_round_trips_through_json() {
    // The adjacently-tagged union must survive a TOML -> doc -> JSON -> doc trip
    // unchanged (the serde tag is robust across both formats; never `untagged`).
    let url = "https://youtu.be/abcdEFGH123";
    let doc = config_with_source(&format!("kind = \"youtube\"\nurl = \"{url}\""));
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse toml");
    let json = serde_json::to_string(&cfg).expect("to json");
    let back: MultiviewConfig = serde_json::from_str(&json).expect("from json");
    assert_eq!(cfg, back, "youtube source must json round-trip losslessly");
    assert!(
        matches!(back.sources[0].kind, SourceKind::Youtube { .. }),
        "the round-tripped kind is still Youtube"
    );
}
