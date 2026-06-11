//! `Output::Display` config-schema tests (DEV-B1 / ADR-0044): the local
//! DRM/KMS display-head output. Pure serde + validation — TOML/JSON
//! round-trip, the five exhaustive `Output` accessors, and the per-item
//! validation rejections. No engine, no hardware.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{DisplayModeSpec, Output, OutputAudioMode, TrackDelivery};
use multiview_core::time::Rational;

// ---------------------------------------------------------------------------
// Deserialization + round-trip
// ---------------------------------------------------------------------------

#[test]
fn display_minimal_deserializes_with_auto_connector() {
    // A bare `kind = "display"` output: the connector defaults to `"auto"`
    // (first connected connector), no mode override, no forced mode.
    let toml_str = r#"
kind = "display"
"#;
    let out: Output = toml::from_str(toml_str).expect("minimal display output");
    match &out {
        Output::Display {
            id,
            connector,
            mode,
            forced_mode,
            gpu_pin,
            audio,
        } => {
            assert_eq!(*id, None);
            assert_eq!(connector, "auto");
            assert_eq!(*mode, None);
            assert_eq!(*forced_mode, None);
            assert!(gpu_pin.is_none());
            assert!(audio.is_none());
        }
        other => panic!("expected Display, got {other:?}"),
    }
}

#[test]
fn display_full_deserializes_and_roundtrips_toml() {
    let toml_str = r#"
kind = "display"
id = "out-monitor-left"
connector = "DP-1"
gpu_pin = { vendor = "amd", stable_id = "0000:00:01.0" }
audio = { mode = "program" }

[mode]
width = 1920
height = 1080
refresh = "60000/1001"
"#;
    let original: Output = toml::from_str(toml_str).expect("full display output");
    match &original {
        Output::Display {
            id,
            connector,
            mode,
            forced_mode,
            gpu_pin,
            audio,
        } => {
            assert_eq!(id.as_deref(), Some("out-monitor-left"));
            assert_eq!(connector, "DP-1");
            let mode = mode.as_ref().expect("mode override present");
            assert_eq!(mode.width, 1920);
            assert_eq!(mode.height, 1080);
            assert_eq!(mode.refresh.rational(), Rational::new(60_000, 1001));
            assert_eq!(*forced_mode, None);
            assert!(gpu_pin.is_some());
            assert_eq!(
                audio.as_ref().map(|a| a.mode),
                Some(OutputAudioMode::Program)
            );
        }
        other => panic!("expected Display, got {other:?}"),
    }
    // serialize -> deserialize is the identity.
    let reparsed: Output =
        toml::from_str(&toml::to_string(&original).expect("serialize")).expect("re-parse");
    assert_eq!(original, reparsed);
}

#[test]
fn display_forced_mode_roundtrips_json() {
    // The EDID-less head case (the field-verified t630 chain): a forced CVT-RB
    // mode authored per connector — never a kernel-cmdline hack.
    let json_in = r#"{
        "kind": "display",
        "connector": "DP-2",
        "forced_mode": { "width": 1920, "height": 1080, "refresh": "50/1" }
    }"#;
    let original: Output = serde_json::from_str(json_in).expect("forced-mode display");
    match &original {
        Output::Display {
            connector,
            mode,
            forced_mode,
            ..
        } => {
            assert_eq!(connector, "DP-2");
            assert_eq!(*mode, None);
            let forced = forced_mode.as_ref().expect("forced mode present");
            assert_eq!(forced.width, 1920);
            assert_eq!(forced.height, 1080);
            assert_eq!(forced.refresh.rational(), Rational::new(50, 1));
        }
        other => panic!("expected Display, got {other:?}"),
    }
    let json_out = serde_json::to_string(&original).expect("serialize");
    let reparsed: Output = serde_json::from_str(&json_out).expect("re-parse");
    assert_eq!(original, reparsed);
}

#[test]
fn display_refresh_rejects_float_fps() {
    // Invariant #3: refresh is an exact-rational string, never a float.
    let toml_str = r#"
kind = "display"
connector = "DP-1"

[mode]
width = 1920
height = 1080
refresh = 59.94
"#;
    assert!(toml::from_str::<Output>(toml_str).is_err());
}

// ---------------------------------------------------------------------------
// The exhaustive Output accessors (the same-crate match wiring)
// ---------------------------------------------------------------------------

#[test]
fn display_accessors_thread_through_the_output_matches() {
    let toml_str = r#"
kind = "display"
id = "head-a"
connector = "HDMI-A-1"
gpu_pin = { vendor = "intel", stable_id = "0000:00:02.0" }
audio = { mode = "program" }
"#;
    let out: Output = toml::from_str(toml_str).expect("display output");
    // explicit_id + id
    assert_eq!(out.explicit_id(), Some("head-a"));
    assert_eq!(out.id(), "head-a");
    // gpu_pin
    assert_eq!(
        out.gpu_pin().map(|p| p.stable_id.as_str()),
        Some("0000:00:02.0")
    );
    // audio
    assert_eq!(out.audio().map(|a| a.mode), Some(OutputAudioMode::Program));
    // label names the kind + connector
    assert_eq!(out.label(), "display HDMI-A-1");
    // audio capability: one LPCM channel-map feed, never selectable discrete
    // tracks (like NDI / AES67) — a discrete-track route is a capability error.
    assert_eq!(out.audio_capability().delivery, TrackDelivery::None);
}

#[test]
fn display_without_id_derives_it_from_the_label() {
    let out: Output =
        toml::from_str("kind = \"display\"\nconnector = \"DP-3\"\n").expect("display output");
    assert_eq!(out.explicit_id(), None);
    assert_eq!(out.id(), "display DP-3");
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Wrap one output TOML fragment in a minimal valid document.
fn doc_with_output(output_toml: &str) -> String {
    format!(
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
{output_toml}
"##
    )
}

fn validate_doc(output_toml: &str) -> Result<(), String> {
    let doc = doc_with_output(output_toml);
    let cfg = multiview_config::MultiviewConfig::load_from_toml(&doc)
        .map_err(|e| format!("parse: {e}"))?;
    cfg.validate().map_err(|e| e.to_string())
}

#[test]
fn display_valid_documents_pass_validation() {
    validate_doc("kind = \"display\"\nconnector = \"DP-1\"").expect("auto-mode display is valid");
    validate_doc(
        "kind = \"display\"\nconnector = \"auto\"\n\
         forced_mode = { width = 1920, height = 1080, refresh = \"50/1\" }",
    )
    .expect("forced-mode display is valid");
}

#[test]
fn display_rejects_empty_connector() {
    let err = validate_doc("kind = \"display\"\nconnector = \"\"").expect_err("empty connector");
    assert!(
        err.contains("connector"),
        "error must name the connector field: {err}"
    );
}

#[test]
fn display_rejects_mode_and_forced_mode_together() {
    let err = validate_doc(
        "kind = \"display\"\nconnector = \"DP-1\"\n\
         mode = { width = 1920, height = 1080, refresh = \"60/1\" }\n\
         forced_mode = { width = 1280, height = 720, refresh = \"60/1\" }",
    )
    .expect_err("mode + forced_mode together is ambiguous");
    assert!(
        err.contains("forced_mode"),
        "error must name the conflict: {err}"
    );
}

#[test]
fn display_rejects_zero_dimensions_and_nonpositive_refresh() {
    let err = validate_doc(
        "kind = \"display\"\nconnector = \"DP-1\"\n\
         mode = { width = 0, height = 1080, refresh = \"60/1\" }",
    )
    .expect_err("zero width");
    assert!(err.contains("width"), "error must name width: {err}");

    let err = validate_doc(
        "kind = \"display\"\nconnector = \"DP-1\"\n\
         forced_mode = { width = 1920, height = 0, refresh = \"60/1\" }",
    )
    .expect_err("zero height");
    assert!(err.contains("height"), "error must name height: {err}");

    let err = validate_doc(
        "kind = \"display\"\nconnector = \"DP-1\"\n\
         mode = { width = 1920, height = 1080, refresh = \"0/1\" }",
    )
    .expect_err("zero refresh");
    assert!(err.contains("refresh"), "error must name refresh: {err}");
}

#[test]
fn display_mode_spec_is_directly_constructible_from_toml() {
    let spec: DisplayModeSpec =
        toml::from_str("width = 1280\nheight = 720\nrefresh = \"60000/1001\"")
            .expect("mode spec parses");
    assert_eq!(spec.width, 1280);
    assert_eq!(spec.height, 720);
    assert_eq!(spec.refresh.rational(), Rational::new(60_000, 1001));
}
