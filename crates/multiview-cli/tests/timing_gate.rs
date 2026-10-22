//! `[timing].ptp_phc` build-gate tests (review finding 6, DEV-C1): a config
//! that asks for a PTP Hardware Clock must FAIL the run in a build without
//! the `ptp` feature — clearly, at startup — never be silently downgraded to
//! the system clock (the B1 display-output precedent: a configured capability
//! the binary cannot provide is an error, not a warning). With the feature,
//! the same check passes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::timing_gate::ensure_ptp_phc_supported;
use multiview_config::MultiviewConfig;

/// A minimal valid document; `timing_toml` is appended verbatim.
fn config_with_timing(timing_toml: &str) -> MultiviewConfig {
    let doc = format!(
        r##"
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
kind = "preset"
preset = "2x2"

[[sources]]
id = "in_a"
kind = "bars"

[[cells]]
id = "cell_a"
rect = {{ x = 0.0, y = 0.0, w = 1.0, h = 1.0 }}
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"

{timing_toml}
"##
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

#[cfg(not(feature = "ptp"))]
#[test]
fn a_configured_ptp_phc_fails_the_gate_without_the_feature() {
    let cfg = config_with_timing("[timing]\nptp_phc = \"/dev/ptp0\"\n");
    let err = ensure_ptp_phc_supported(cfg.timing.as_ref())
        .expect_err("ptp_phc in a non-ptp build must fail, never be silently ignored");
    assert!(
        err.contains("[timing].ptp_phc requires a ptp build"),
        "the error must state the contract, got: {err}"
    );
    assert!(
        err.contains("/dev/ptp0"),
        "the error must name the configured device, got: {err}"
    );
}

#[cfg(feature = "ptp")]
#[test]
fn a_configured_ptp_phc_passes_the_gate_with_the_feature() {
    let cfg = config_with_timing("[timing]\nptp_phc = \"/dev/ptp0\"\n");
    ensure_ptp_phc_supported(cfg.timing.as_ref())
        .expect("a ptp build samples the configured PHC at run time");
}

#[test]
fn no_ptp_phc_always_passes_the_gate() {
    assert!(
        ensure_ptp_phc_supported(None).is_ok(),
        "no [timing] block never trips the gate"
    );
    let cfg = config_with_timing("[timing]\nlink_offset_ms = 150\n");
    assert!(
        ensure_ptp_phc_supported(cfg.timing.as_ref()).is_ok(),
        "a [timing] block without ptp_phc never trips the gate"
    );
}
