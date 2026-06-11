//! Schema + validation tests for the `[discovery]` config section (DEV-A5
//! review FINDING 5): the operator-configured zowietek-control DNS-SD service
//! type and the extra browse types. The zowietek vendor's control-API mDNS
//! service type is **unverified**, so it is only ever recognised when the
//! operator configures it here — never fabricated from a built-in constant.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::MultiviewConfig;

/// A minimal, valid one-cell document the `[discovery]` fragments are appended
/// to (mirrors `tests/devices.rs`).
const BASE: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
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
kind = "bars"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "rtsp_server"
id = "out-main"
mount = "/multiview"
codec = "h264"
"##;

#[test]
fn discovery_section_parses_validates_and_roundtrips() {
    let toml = format!(
        "{BASE}\n[discovery]\nzowietek_service_type = \"_zowietek-ctl._tcp.local.\"\n\
         extra_service_types = [\"_extra._udp\"]\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&toml).expect("the [discovery] section parses");
    cfg.validate().expect("a well-formed [discovery] validates");

    let discovery = cfg
        .discovery
        .as_ref()
        .expect("the discovery section is kept");
    assert_eq!(
        discovery.zowietek_service_type.as_deref(),
        Some("_zowietek-ctl._tcp.local.")
    );
    assert_eq!(
        discovery.extra_service_types,
        vec!["_extra._udp".to_owned()]
    );

    // TOML -> JSON -> back keeps the section losslessly.
    let json = cfg.to_json().expect("serializes to JSON");
    let back = MultiviewConfig::load_from_json(&json).expect("JSON reparses");
    assert_eq!(back.discovery, cfg.discovery);
}

#[test]
fn absent_discovery_section_is_none_and_valid() {
    let cfg = MultiviewConfig::load_from_toml(BASE).expect("the base document parses");
    cfg.validate().expect("the base document validates");
    assert!(
        cfg.discovery.is_none(),
        "no [discovery] section means no configured discovery"
    );
}

#[test]
fn discovery_rejects_an_empty_zowietek_service_type() {
    let toml = format!("{BASE}\n[discovery]\nzowietek_service_type = \"\"\n");
    let cfg = MultiviewConfig::load_from_toml(&toml).expect("parses");
    let err = cfg
        .validate()
        .expect_err("an empty service type must be rejected");
    assert!(
        err.to_string().contains("discovery"),
        "the error names the discovery section: {err}"
    );
}

#[test]
fn discovery_rejects_a_malformed_extra_service_type() {
    // A DNS-SD service type is `_name._tcp` / `_name._udp` (optionally
    // `.local.`-suffixed). A bare hostname-ish string is a typo, not a type.
    let toml = format!("{BASE}\n[discovery]\nextra_service_types = [\"zowietek.local\"]\n");
    let cfg = MultiviewConfig::load_from_toml(&toml).expect("parses");
    let err = cfg
        .validate()
        .expect_err("a malformed DNS-SD service type must be rejected");
    assert!(
        err.to_string().contains("discovery"),
        "the error names the discovery section: {err}"
    );
}

#[test]
fn discovery_accepts_udp_types_and_types_without_local_suffix() {
    let toml = format!(
        "{BASE}\n[discovery]\nzowietek_service_type = \"_zow._udp\"\n\
         extra_service_types = [\"_x._tcp\", \"_y._udp.local.\"]\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&toml).expect("parses");
    cfg.validate()
        .expect("both protocols, with or without .local., are valid");
}
