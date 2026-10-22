//! The `control.cast_media_base` knob (DEV-D2, ADR-M011): the
//! externally-reachable base URL Cast media URLs are derived from. Cast
//! devices ignore DHCP-provided DNS (they resolve via hardcoded public
//! resolvers) and cannot reach a loopback, so the operator names the address
//! the DEVICE can fetch from — an IP literal (or a publicly resolvable name);
//! the deep host validation lives with the cast driver, the config layer
//! enforces shape (http/https, non-empty).
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
fn cast_media_base_is_optional_and_round_trips() {
    // Absent: today's behaviour, no cast delivery derivable.
    let cfg = MultiviewConfig::load_from_toml(&base("")).unwrap();
    cfg.validate().expect("no cast_media_base is valid");
    assert_eq!(
        cfg.control.as_ref().and_then(|c| c.cast_media_base.clone()),
        None
    );

    // Present: parses, validates, and round-trips through TOML + JSON.
    let cfg =
        MultiviewConfig::load_from_toml(&base("cast_media_base = \"http://192.0.2.7:8080\"\n"))
            .unwrap();
    cfg.validate().expect("an IPv4-literal base validates");
    assert_eq!(
        cfg.control
            .as_ref()
            .and_then(|c| c.cast_media_base.as_deref()),
        Some("http://192.0.2.7:8080")
    );
    let toml_text = cfg.to_toml().expect("serializes");
    let reparsed = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses");
    assert_eq!(cfg, reparsed);
}

#[test]
fn cast_media_base_accepts_a_bracketed_ipv6_literal() {
    // IPv6-first examples lead (ADR-0042); Cast's IPv4-in-practice reality is
    // a legacy-interop note for hardware validation, never a config rule.
    let cfg =
        MultiviewConfig::load_from_toml(&base("cast_media_base = \"http://[2001:db8::7]:8080\"\n"))
            .unwrap();
    cfg.validate().expect("an IPv6-literal base validates");
}

#[test]
fn cast_media_base_must_be_http_or_https() {
    let cfg =
        MultiviewConfig::load_from_toml(&base("cast_media_base = \"rtsp://192.0.2.7:8554\"\n"))
            .unwrap();
    let err = cfg.validate().expect_err("non-HTTP scheme is rejected");
    let msg = err.to_string();
    assert!(msg.contains("cast_media_base"), "names the field: {msg}");
}

#[test]
fn cast_media_base_must_not_be_empty() {
    let cfg = MultiviewConfig::load_from_toml(&base("cast_media_base = \"\"\n")).unwrap();
    let err = cfg.validate().expect_err("an empty base is rejected");
    assert!(
        err.to_string().contains("cast_media_base"),
        "names the field: {err}"
    );
}
