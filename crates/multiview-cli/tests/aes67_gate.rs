//! AES67 / ST 2110-30 build-gate tests (#103, ADR-0033): a config declaring an
//! `Output::Aes67` or a `SourceKind::Aes67` must FAIL the runnable check with a
//! clear, actionable error in a build without the `aes67` feature — never be
//! silently skipped (the `display-kms` precedent). With the feature, the same
//! check passes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::outputs::{ensure_aes67_outputs_supported, ensure_aes67_sources_supported};
use multiview_config::MultiviewConfig;

fn config_with_aes67_output() -> MultiviewConfig {
    // A bars source in a cell (a valid layout) plus an AES67 raw-PCM output.
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
kind = "aes67"
label = "program-audio"
multicast = "[ff3e::1]:5004"

[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
"##;
    let cfg = MultiviewConfig::load_from_toml(doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

fn config_with_aes67_source() -> MultiviewConfig {
    // A bars source placed in the cell plus an audio-only AES67 source that is NOT
    // placed in any cell (validates fine — cells reference sources, not vice-versa).
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

[[sources]]
id = "aud_in"
kind = "aes67"
sdp = "v=0"

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
    cfg.validate()
        .expect("document validates (an audio-only aes67 source needs no cell)");
    cfg
}

#[cfg(not(feature = "aes67"))]
#[test]
fn aes67_output_fails_the_gate_without_the_feature() {
    let cfg = config_with_aes67_output();
    let err = ensure_aes67_outputs_supported(&cfg.outputs)
        .expect_err("an aes67 output must fail, never be skipped");
    assert!(
        err.contains("aes67"),
        "the error must name the required build feature: {err}"
    );
}

#[cfg(not(feature = "aes67"))]
#[test]
fn aes67_source_fails_the_gate_without_the_feature() {
    let cfg = config_with_aes67_source();
    let err = ensure_aes67_sources_supported(&cfg.sources)
        .expect_err("an aes67 source must fail, never be skipped");
    assert!(
        err.contains("aes67"),
        "the error must name the required build feature: {err}"
    );
}

#[cfg(feature = "aes67")]
#[test]
fn aes67_output_and_source_pass_the_gate_with_the_feature() {
    let out_cfg = config_with_aes67_output();
    ensure_aes67_outputs_supported(&out_cfg.outputs)
        .expect("aes67 outputs are runnable in an aes67 build");
    let src_cfg = config_with_aes67_source();
    ensure_aes67_sources_supported(&src_cfg.sources)
        .expect("aes67 sources are runnable in an aes67 build");
}

#[test]
fn non_aes67_config_always_passes_the_gate() {
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
    ensure_aes67_outputs_supported(&cfg.outputs).expect("no aes67 outputs => always fine");
    ensure_aes67_sources_supported(&cfg.sources).expect("no aes67 sources => always fine");
}
