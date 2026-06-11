//! Display-output build-gate tests (DEV-B1 / ADR-0044): a config declaring an
//! `Output::Display` must FAIL the runnable-outputs check with a clear,
//! actionable error in a build without the `display-kms` feature — never be
//! silently skipped. With the feature, the same check passes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::outputs::ensure_display_outputs_supported;
use multiview_config::MultiviewConfig;

fn config_with_display_output() -> MultiviewConfig {
    let doc = r##"
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
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "display"
connector = "DP-1"

[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
"##;
    let cfg = MultiviewConfig::load_from_toml(doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

#[cfg(not(feature = "display-kms"))]
#[test]
fn display_output_fails_the_gate_without_the_feature() {
    let cfg = config_with_display_output();
    let err = ensure_display_outputs_supported(&cfg.outputs)
        .expect_err("a display output must fail, never be skipped");
    assert!(
        err.contains("display-kms"),
        "the error must name the required build feature: {err}"
    );
    assert!(
        err.contains("display"),
        "the error must name the output kind: {err}"
    );
}

#[cfg(feature = "display-kms")]
#[test]
fn display_output_passes_the_gate_with_the_feature() {
    let cfg = config_with_display_output();
    ensure_display_outputs_supported(&cfg.outputs)
        .expect("display outputs are runnable in a display-kms build");
}

#[test]
fn non_display_outputs_always_pass_the_gate() {
    let doc = r##"
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
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
"##;
    let cfg = MultiviewConfig::load_from_toml(doc).expect("document parses");
    ensure_display_outputs_supported(&cfg.outputs).expect("no display outputs => always fine");
}
