//! `[iptv]` publication section: optional, round-trips, validates structure.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{IptvConfig, IptvEntryKind, MultiviewConfig};

const BASE: &str = r##"schema_version = 1
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
[[sources]]
id = "cnn"
kind = "hls"
url = "https://cdn.example/cnn.m3u8"
[[cells]]
id = "c"
area = "a"
[cells.source]
input_id = "cnn"
[[outputs]]
kind = "hls"
path = "/tmp/x/program.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

#[test]
fn absent_section_is_none_and_not_serialized() {
    let config = MultiviewConfig::load_from_toml(BASE).unwrap();
    assert!(config.iptv.is_none());
    let json = serde_json::to_value(&config).unwrap();
    assert!(json.get("iptv").is_none());
}

#[test]
fn section_round_trips_toml_and_json() {
    let text = format!(
        "{BASE}[iptv]\nenabled = true\n[[iptv.entries]]\nkind = \"source\"\nref = \"cnn\"\ntvg_id = \"CNN.us\"\nallow_direct_source_url = true\n[[iptv.entries]]\nkind = \"output\"\nref = \"output-0\"\nname = \"Program\"\n"
    );
    let config = MultiviewConfig::load_from_toml(&text).unwrap();
    config.validate().unwrap();
    let iptv = config.iptv.clone().unwrap();
    assert!(iptv.enabled);
    assert_eq!(iptv.entries[0].kind, IptvEntryKind::Source);
    assert_eq!(iptv.entries[0].reference, "cnn");
    assert_eq!(iptv.entries[1].name.as_deref(), Some("Program"));
    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(json["iptv"]["entries"][0]["ref"], "cnn");
    let back: MultiviewConfig = serde_json::from_value(json).unwrap();
    assert_eq!(back, config);
}

#[test]
fn defaults_are_disabled_and_structural_errors_reject() {
    let empty: IptvConfig = serde_json::from_str("{}").unwrap();
    assert!(!empty.enabled);
    assert_eq!(empty.entries.len(), 0);
    for bad in [
        r#"{"entries":[{"kind":"source","ref":"cnn"}]}"#,
        r#"{"entries":[{"kind":"output","ref":"a"},{"kind":"output","ref":"a"}]}"#,
        r#"{"entries":[{"kind":"output","ref":"a","name":"x\"y"}]}"#,
        r#"{"entries":[{"kind":"output","ref":"a","logo":"http://u:p@h/l.png"}]}"#,
        r#"{"public_base_url":"ftp://h"}"#,
    ] {
        let config: IptvConfig = serde_json::from_str(bad).unwrap();
        assert!(
            config
                .validate(Some(&["cnn"]), Some(&[("a", true)]))
                .is_err(),
            "{bad}"
        );
    }
    assert!(serde_json::from_str::<IptvConfig>(r#"{"bogus":1}"#).is_err());
}

#[test]
fn reference_resolution_is_caller_driven() {
    let config: IptvConfig = serde_json::from_str(
        r#"{"entries":[{"kind":"source","ref":"gone","allow_direct_source_url":true},{"kind":"output","ref":"rtmp"}]}"#,
    )
    .unwrap();
    // Structural-only (document validation): deleting a source never blocks.
    config.validate(None, None).unwrap();
    assert!(config.validate(Some(&["cnn"]), None).is_err());
    assert!(config.validate(None, Some(&[("rtmp", false)])).is_err());
}
