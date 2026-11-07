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

#[test]
fn hls_headers_and_origin_round_trip_and_validate() {
    let s = source(serde_json::json!({
        "id": "k", "kind": "hls", "url": "https://h/x.m3u8",
        "user_agent": "Mozilla/5.0", "referrer": "https://h/",
        "origin": { "provider": "iptv-org", "channel": "KAKE.us", "feed": "SD",
                    "stream_url": "https://h/x.m3u8", "labels": ["Geo-blocked"],
                    "linked_at": "2026-11-07T05:00:00Z" }
    }));
    s.validate().expect("valid");
    let origin = s.origin.as_ref().expect("origin");
    assert_eq!(origin.follow, multiview_config::OriginFollow::Notify);
    let toml = toml::to_string(&s).expect("toml");
    assert!(
        toml.contains("[origin]") && toml.contains("user_agent"),
        "{toml}"
    );
    let back: Source = toml::from_str(&toml).expect("round trip");
    assert_eq!(back, s);
}

#[test]
fn source_without_origin_serializes_no_origin_block() {
    let s = source(serde_json::json!({ "id": "k", "kind": "hls", "url": "https://h/x" }));
    let toml = toml::to_string(&s).expect("toml");
    assert!(
        !toml.contains("origin") && !toml.contains("user_agent"),
        "{toml}"
    );
}

#[test]
fn hls_header_with_crlf_or_oversize_fails_validation() {
    for (field, value) in [
        ("user_agent", "bad\r\nX-Evil: 1".to_owned()),
        ("referrer", "bad\nvalue".to_owned()),
        ("user_agent", "a".repeat(1025)),
        ("user_agent", String::new()),
    ] {
        let s = source(serde_json::json!({
            "id": "k", "kind": "hls", "url": "https://h/x", field: value
        }));
        let err = s.validate().expect_err("must fail");
        assert!(err.to_string().contains(field), "{err}");
    }
    let ok = source(serde_json::json!({
        "id": "k", "kind": "hls", "url": "https://h/x", "user_agent": "a".repeat(1024)
    }));
    ok.validate().expect("1 KiB is allowed");
}
