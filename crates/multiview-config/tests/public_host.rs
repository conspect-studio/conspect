//! The `control.public_host` knob (ADR-0101, "a link to copy the actual URL
//! to paste into a tool like VLC"): the externally-reachable **bare**
//! hostname/IP-literal viewer playback URLs are derived against, overriding
//! the request `Host` header fallback. Unlike `cast_media_base` this carries
//! no scheme and no port (mirrors `cast_control.rs`'s structure).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::MultiviewConfig;

/// A minimal valid document with a `[control]` section.
fn base(control_extra: &str) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
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
fit = "contain"
[cells.source]
input_id = "in_a"

[[outputs]]
id = "out-a"
kind = "hls"
path = "/srv/hls/out-a/a.m3u8"
codec = "mpeg2video"
segment_ms = 2000

[control]
listen = "[::]:8080"
{control_extra}
"##
    )
}

#[test]
fn public_host_is_optional_and_round_trips() {
    // Absent: falls back to the request Host header at serve time.
    let cfg = MultiviewConfig::load_from_toml(&base("")).unwrap();
    cfg.validate().expect("no public_host is valid");
    assert_eq!(
        cfg.control.as_ref().and_then(|c| c.public_host.clone()),
        None
    );

    // Present: a bare hostname validates and round-trips through TOML.
    let cfg = MultiviewConfig::load_from_toml(&base("public_host = \"viewer.example\"\n")).unwrap();
    cfg.validate().expect("a bare hostname validates");
    assert_eq!(
        cfg.control.as_ref().and_then(|c| c.public_host.as_deref()),
        Some("viewer.example")
    );
    let toml_text = cfg.to_toml().expect("serializes");
    let reparsed = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses");
    assert_eq!(cfg, reparsed);
}

#[test]
fn public_host_accepts_an_ipv4_literal() {
    let cfg = MultiviewConfig::load_from_toml(&base("public_host = \"203.0.113.9\"\n")).unwrap();
    cfg.validate().expect("an IPv4 literal validates");
}

#[test]
fn public_host_accepts_a_bracketed_ipv6_literal() {
    // IPv6-first examples lead (ADR-0042).
    let cfg = MultiviewConfig::load_from_toml(&base("public_host = \"[2001:db8::7]\"\n")).unwrap();
    cfg.validate().expect("a bracketed IPv6 literal validates");
}

#[test]
fn public_host_must_not_be_empty() {
    let cfg = MultiviewConfig::load_from_toml(&base("public_host = \"\"\n")).unwrap();
    let err = cfg
        .validate()
        .expect_err("an empty public_host is rejected");
    assert!(
        err.to_string().contains("public_host"),
        "names the field: {err}"
    );
}

#[test]
fn public_host_must_not_carry_a_scheme() {
    let cfg = MultiviewConfig::load_from_toml(&base("public_host = \"http://viewer.example\"\n"))
        .unwrap();
    let err = cfg.validate().expect_err("a scheme is rejected");
    assert!(
        err.to_string().contains("public_host"),
        "names the field: {err}"
    );
}

#[test]
fn public_host_must_not_carry_a_port() {
    let cfg =
        MultiviewConfig::load_from_toml(&base("public_host = \"viewer.example:8443\"\n")).unwrap();
    let err = cfg.validate().expect_err("a port is rejected");
    assert!(
        err.to_string().contains("public_host"),
        "names the field: {err}"
    );
}

#[test]
fn public_host_must_not_carry_a_path() {
    let cfg =
        MultiviewConfig::load_from_toml(&base("public_host = \"viewer.example/x\"\n")).unwrap();
    let err = cfg.validate().expect_err("a path is rejected");
    assert!(
        err.to_string().contains("public_host"),
        "names the field: {err}"
    );
}
