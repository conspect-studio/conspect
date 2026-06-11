//! Per-item semantic validation on `Source`/`Output` (ADR-W015 / review M3):
//! the same checks `MultiviewConfig::validate()` applies per resource, exposed
//! so the control plane can reject a bad document at the API boundary instead
//! of poisoning the composed export.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{Output, Source};

fn source(json: serde_json::Value) -> Source {
    serde_json::from_value(json).expect("structurally valid source")
}

fn output(json: serde_json::Value) -> Output {
    serde_json::from_value(json).expect("structurally valid output")
}

#[test]
fn solid_source_with_bad_hex_fails_validation() {
    let s = source(serde_json::json!({ "id": "s", "kind": "solid", "color": "chartreuse" }));
    let err = s.validate().expect_err("bad hex must fail");
    assert!(err.to_string().contains("hex"), "{err}");
}

#[test]
fn clock_source_with_out_of_range_tz_fails_validation() {
    let s = source(serde_json::json!({ "id": "s", "kind": "clock", "tz_offset_minutes": 99999 }));
    let err = s.validate().expect_err("tz out of range must fail");
    assert!(err.to_string().contains("tz_offset_minutes"), "{err}");
}

#[test]
fn valid_sources_pass_validation() {
    source(serde_json::json!({ "id": "s", "kind": "solid", "color": "#101014" }))
        .validate()
        .expect("valid solid");
    source(serde_json::json!({ "id": "s", "kind": "clock", "tz_offset_minutes": 600 }))
        .validate()
        .expect("valid clock");
    source(serde_json::json!({ "id": "s", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/x" }))
        .validate()
        .expect("valid rtsp");
}

#[test]
fn empty_source_id_fails_validation() {
    let s = source(serde_json::json!({ "id": "", "kind": "bars" }));
    assert!(s.validate().is_err(), "empty id must fail");
}

#[test]
fn output_with_empty_codec_fails_validation() {
    let o = output(serde_json::json!({ "kind": "rtmp", "url": "rtmp://h/x", "codec": "" }));
    let err = o.validate().expect_err("empty codec must fail");
    assert!(err.to_string().contains("codec"), "{err}");
}

#[test]
fn output_with_empty_explicit_id_fails_validation() {
    let o = output(serde_json::json!({ "id": "", "kind": "ndi", "name": "MV" }));
    assert!(o.validate().is_err(), "empty explicit id must fail");
}

#[test]
fn valid_outputs_pass_validation() {
    output(serde_json::json!({ "kind": "hls", "path": "/srv/hls", "codec": "h264" }))
        .validate()
        .expect("valid hls");
    output(serde_json::json!({ "kind": "ndi", "name": "MV" }))
        .validate()
        .expect("valid ndi (no codec field exists)");
}
