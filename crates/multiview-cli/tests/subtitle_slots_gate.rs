//! Subtitle-slot build-gate tests (#508): an output that declares subtitle
//! slots must FAIL the startup/validate check, clearly, in a build without the
//! `overlay` feature. Without `overlay` no caption source is wired, so the
//! slot's `WebVTT` rendition would be published with every segment empty (the
//! `0 cues` of the local cast proof in #508). With the feature the same check
//! passes. A config without subtitle slots always passes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::outputs::ensure_subtitle_slots_supported;
use multiview_config::MultiviewConfig;

/// A minimal valid document with one cast output; `slots_toml` is appended
/// after it verbatim.
fn config_with_cast(slots_toml: &str) -> MultiviewConfig {
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
kind = "cast"
id = "cast-1"
{slots_toml}
"##
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

const SUBTITLE_SLOT: &str = r#"
[[outputs.slots.subtitle]]
name = "English"
language = "en"
source = "in_a"
"#;

#[cfg(not(feature = "overlay"))]
#[test]
fn subtitle_slots_fail_the_gate_without_overlay() {
    let cfg = config_with_cast(SUBTITLE_SLOT);
    let err = ensure_subtitle_slots_supported(&cfg.outputs)
        .expect_err("subtitle slots must be refused in a non-overlay build");
    assert!(err.contains("overlay"), "names the feature: {err}");
    assert!(err.contains("cast-1"), "names the output: {err}");
    assert!(err.contains("no cues"), "says what would happen: {err}");
}

#[cfg(feature = "overlay")]
#[test]
fn subtitle_slots_pass_the_gate_with_overlay() {
    let cfg = config_with_cast(SUBTITLE_SLOT);
    ensure_subtitle_slots_supported(&cfg.outputs).expect("overlay build carries subtitle slots");
}

#[test]
fn outputs_without_subtitle_slots_always_pass() {
    let cfg = config_with_cast("");
    ensure_subtitle_slots_supported(&cfg.outputs).expect("no subtitle slots => always fine");
    let cfg = config_with_cast("[[outputs.slots.audio]]\nlanguage = \"en\"\n");
    ensure_subtitle_slots_supported(&cfg.outputs).expect("audio slots only => always fine");
}
