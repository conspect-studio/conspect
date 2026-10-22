//! OUTMETA config-schema tests (ADR-0088 output metadata + ADR-0089 output
//! orientation): the additive per-output `metadata` / `orientation` surfaces,
//! their TOML/JSON round-trip, validation (`service_id` range, ISO-639-2
//! language, `mode = "tag"` rejected on tag-less transports), the per-transport
//! capability matrices, and the projection plans (`Applied`/`Dropped`).
//!
//! Pure serde + pure logic — no engine, no ffmpeg, no network. Load-bearing
//! properties: metadata is operator *intent* projected onto whatever the
//! transport can carry (unsupported ⇒ a visible `Dropped`, never a silent
//! no-op); orientation reuses core `QuarterTurn`; a flip forces the pixels path;
//! `tag` is invalid where no rotation tag exists (MPEG-TS/RTSP/NDI).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    MetadataField, OrientationMechanism, OrientationMode, OrientationTagCapability, Output,
    OutputFlip, OutputMetadata,
};
use multiview_core::layout::QuarterTurn;

// ---------------------------------------------------------------------------
// Metadata — parse + round-trip
// ---------------------------------------------------------------------------

#[test]
fn metadata_parses_all_fields_on_hls() {
    let toml_str = r#"
kind = "hls"
path = "/hls/multiview"
codec = "h264"

[metadata]
title = "Studio A Multiview"
provider = "Aperim Newsroom"
language = "eng"
service_id = 1
description = "Gallery confidence feed"
timed = { id3 = true, daterange = true }
"#;
    let out: Output = toml::from_str(toml_str).expect("valid HLS output with metadata");
    let meta = out.metadata().expect("metadata present");
    assert_eq!(meta.title.as_deref(), Some("Studio A Multiview"));
    assert_eq!(meta.provider.as_deref(), Some("Aperim Newsroom"));
    assert_eq!(meta.language.as_deref(), Some("eng"));
    assert_eq!(meta.service_id, Some(1));
    assert_eq!(meta.description.as_deref(), Some("Gallery confidence feed"));
    let timed = meta.timed.expect("timed block present");
    assert!(timed.id3);
    assert!(timed.daterange);
}

#[test]
fn metadata_absent_is_none() {
    let toml_str = r#"
kind = "hls"
path = "/hls/multiview"
codec = "h264"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid HLS output, no metadata");
    assert!(out.metadata().is_none(), "absent ⇒ None");
}

#[test]
fn metadata_json_roundtrips() {
    let out = Output::Hls {
        id: None,
        path: "/hls/multiview".to_owned(),
        codec: "h264".to_owned(),
        segment_ms: None,
        gpu_pin: None,
        audio: None,
        metadata: Some(OutputMetadata::new(
            Some("Title".to_owned()),
            None,
            Some("fra".to_owned()),
            None,
            None,
            None,
        )),
        orientation: None,
    };
    let json = serde_json::to_string(&out).expect("serialize");
    let back: Output = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(out, back, "JSON round-trip is lossless");
}

#[test]
fn empty_metadata_block_is_empty() {
    // An `[metadata]` table with nothing in it deserializes to a present-but-
    // empty intent; `is_empty()` reports it projects to nothing.
    let toml_str = r#"
kind = "hls"
path = "/hls/multiview"
codec = "h264"

[metadata]
"#;
    let out: Output = toml::from_str(toml_str).expect("valid empty metadata block");
    let meta = out.metadata().expect("present but empty");
    assert!(meta.is_empty(), "no requested field ⇒ empty");
}

// ---------------------------------------------------------------------------
// Metadata — validation (contradictions only)
// ---------------------------------------------------------------------------

#[test]
fn metadata_service_id_zero_is_rejected() {
    let meta = OutputMetadata::new(None, None, None, Some(0), None, None);
    let err = meta
        .validate("out")
        .expect_err("service_id 0 is out of 1..=65535");
    let msg = format!("{err}");
    assert!(msg.contains("service_id"), "names the field: {msg}");
}

#[test]
fn metadata_service_id_too_large_is_rejected() {
    let meta = OutputMetadata::new(None, None, None, Some(65_536), None, None);
    meta.validate("out")
        .expect_err("service_id 65536 is above the DVB max");
}

#[test]
fn metadata_service_id_bounds_are_inclusive() {
    for id in [1, 65_535] {
        let meta = OutputMetadata::new(None, None, None, Some(id), None, None);
        meta.validate("out")
            .unwrap_or_else(|e| panic!("service_id {id} must be valid: {e}"));
    }
}

#[test]
fn metadata_language_must_be_iso_639_2() {
    for bad in ["en", "english", "ENG", "e1g", "en "] {
        let meta = OutputMetadata::new(None, None, Some(bad.to_owned()), None, None, None);
        assert!(
            meta.validate("out").is_err(),
            "language {bad:?} must be rejected (not a 3-letter lowercase ISO-639-2 code)"
        );
    }
    let good = OutputMetadata::new(None, None, Some("eng".to_owned()), None, None, None);
    good.validate("out").expect("\"eng\" is valid ISO-639-2");
}

#[test]
fn metadata_validation_runs_in_document_validate() {
    // The document-level validate() must reject a bad service_id authored on an
    // output (proves the per-output validate is actually wired in).
    let toml_str = r##"
schema_version = 3

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "preset"
preset = "2x2"

[[sources]]
id = "cam_a"
kind = "test"

[[cells]]
id = "cell_a"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "cam_a"

[[outputs]]
kind = "hls"
path = "/hls/mv"
codec = "h264"

[outputs.metadata]
service_id = 70000
"##;
    let cfg: multiview_config::MultiviewConfig =
        toml::from_str(toml_str).expect("parses (validation is separate)");
    let err = cfg
        .validate()
        .expect_err("service_id out of range fails validate");
    assert!(format!("{err}").contains("service_id"));
}

// ---------------------------------------------------------------------------
// Metadata — per-transport capability + projection plan
// ---------------------------------------------------------------------------

fn hls_with(meta: OutputMetadata) -> Output {
    Output::Hls {
        id: None,
        path: "/hls/mv".to_owned(),
        codec: "h264".to_owned(),
        segment_ms: None,
        gpu_pin: None,
        audio: None,
        metadata: Some(meta),
        orientation: None,
    }
}

fn srt_with(meta: OutputMetadata) -> Output {
    Output::Srt {
        id: None,
        url: "srt://[::1]:9000".to_owned(),
        codec: "h264".to_owned(),
        gpu_pin: None,
        audio: None,
        metadata: Some(meta),
        orientation: None,
    }
}

fn rtmp_with(meta: OutputMetadata) -> Output {
    Output::Rtmp {
        id: None,
        url: "rtmp://[::1]/live/key".to_owned(),
        codec: "h264".to_owned(),
        gpu_pin: None,
        multitrack: false,
        audio: None,
        metadata: Some(meta),
        orientation: None,
    }
}

#[test]
fn ts_family_carries_provider_and_service_id() {
    // SRT is MPEG-TS: it carries provider + service_id (the DVB SDT/PMT).
    let meta = OutputMetadata::new(
        Some("T".to_owned()),
        Some("P".to_owned()),
        Some("eng".to_owned()),
        Some(1),
        Some("D".to_owned()),
        None,
    );
    let plan = srt_with(meta).metadata_plan();
    assert!(plan.title.as_ref().unwrap().is_applied());
    assert!(plan.provider.as_ref().unwrap().is_applied());
    assert!(plan.language.as_ref().unwrap().is_applied());
    assert!(plan.service_id.as_ref().unwrap().is_applied());
    assert!(plan.description.as_ref().unwrap().is_applied());
    assert!(plan.dropped().is_empty(), "TS carries every field");
}

#[test]
fn hls_drops_provider_and_service_id() {
    // HLS has no DVB provider/service-id carrier ⇒ those are visibly Dropped,
    // not a validation error and not silently lost.
    let meta = OutputMetadata::new(
        Some("T".to_owned()),
        Some("P".to_owned()),
        Some("eng".to_owned()),
        Some(1),
        Some("D".to_owned()),
        None,
    );
    let plan = hls_with(meta).metadata_plan();
    assert!(plan.title.as_ref().unwrap().is_applied());
    assert!(plan.language.as_ref().unwrap().is_applied());
    assert!(
        matches!(plan.provider, Some(MetadataField::Dropped { .. })),
        "HLS has no provider carrier"
    );
    assert!(
        matches!(plan.service_id, Some(MetadataField::Dropped { .. })),
        "HLS has no DVB service id"
    );
    let dropped: Vec<&str> = plan.dropped().iter().map(|(name, _)| *name).collect();
    assert_eq!(dropped, vec!["provider", "service_id"]);
}

#[test]
fn rtmp_drops_language_and_provider_and_service_id() {
    let meta = OutputMetadata::new(
        Some("T".to_owned()),
        Some("P".to_owned()),
        Some("eng".to_owned()),
        Some(1),
        Some("D".to_owned()),
        None,
    );
    let plan = rtmp_with(meta).metadata_plan();
    assert!(
        plan.title.as_ref().unwrap().is_applied(),
        "onMetaData title"
    );
    assert!(matches!(plan.provider, Some(MetadataField::Dropped { .. })));
    assert!(matches!(plan.language, Some(MetadataField::Dropped { .. })));
    assert!(matches!(
        plan.service_id,
        Some(MetadataField::Dropped { .. })
    ));
}

#[test]
fn plan_only_reports_requested_fields() {
    // Only `title` is requested ⇒ every other plan slot is None (not reported).
    let meta = OutputMetadata::new(Some("T".to_owned()), None, None, None, None, None);
    let plan = hls_with(meta).metadata_plan();
    assert!(plan.title.is_some());
    assert!(plan.provider.is_none());
    assert!(plan.language.is_none());
    assert!(plan.service_id.is_none());
    assert!(plan.description.is_none());
}

#[test]
fn no_metadata_is_empty_plan() {
    let out = Output::Hls {
        id: None,
        path: "/hls/mv".to_owned(),
        codec: "h264".to_owned(),
        segment_ms: None,
        gpu_pin: None,
        audio: None,
        metadata: None,
        orientation: None,
    };
    let plan = out.metadata_plan();
    assert!(plan.dropped().is_empty());
    assert!(plan.title.is_none() && plan.service_id.is_none());
}

// ---------------------------------------------------------------------------
// Orientation — parse + round-trip
// ---------------------------------------------------------------------------

#[test]
fn orientation_parses_turn_mode_flip() {
    let toml_str = r#"
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"

[orientation]
turn = "cw90"
mode = "pixels"
flip = "horizontal"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid orientation");
    let o = out.orientation().expect("orientation present");
    assert_eq!(o.turn, QuarterTurn::Cw90);
    assert_eq!(o.mode, OrientationMode::Pixels);
    assert_eq!(o.flip, OutputFlip::Horizontal);
}

#[test]
fn orientation_defaults_when_partial() {
    // mode defaults to auto, flip defaults to none.
    let toml_str = r#"
kind = "hls"
path = "/hls/mv"
codec = "h264"

[orientation]
turn = "cw180"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid partial orientation");
    let o = out.orientation().expect("present");
    assert_eq!(o.turn, QuarterTurn::Cw180);
    assert_eq!(o.mode, OrientationMode::Auto);
    assert_eq!(o.flip, OutputFlip::None);
}

#[test]
fn orientation_json_roundtrips() {
    let out = Output::Hls {
        id: None,
        path: "/hls/mv".to_owned(),
        codec: "h264".to_owned(),
        segment_ms: None,
        gpu_pin: None,
        audio: None,
        metadata: None,
        orientation: Some(multiview_config::OutputOrientation::new(
            QuarterTurn::Cw270,
            OrientationMode::Tag,
            OutputFlip::None,
        )),
    };
    let json = serde_json::to_string(&out).expect("serialize");
    let back: Output = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(out, back);
}

// ---------------------------------------------------------------------------
// Orientation — validation + mechanism resolution
// ---------------------------------------------------------------------------

#[test]
fn tag_mode_rejected_on_mpegts_transport() {
    // SRT (MPEG-TS), RTSP and NDI carry no rotation tag ⇒ mode = "tag" is a
    // validation error.
    let srt = Output::Srt {
        id: None,
        url: "srt://[::1]:9000".to_owned(),
        codec: "h264".to_owned(),
        gpu_pin: None,
        audio: None,
        metadata: None,
        orientation: Some(multiview_config::OutputOrientation::new(
            QuarterTurn::Cw90,
            OrientationMode::Tag,
            OutputFlip::None,
        )),
    };
    let err = srt.validate().expect_err("tag on TS is rejected");
    assert!(format!("{err}").contains("tag"), "names the mode");
    assert_eq!(
        srt.orientation_tag_capability(),
        OrientationTagCapability::None
    );
}

#[test]
fn tag_mode_accepted_on_hls() {
    let hls = Output::Hls {
        id: None,
        path: "/hls/mv".to_owned(),
        codec: "h264".to_owned(),
        segment_ms: None,
        gpu_pin: None,
        audio: None,
        metadata: None,
        orientation: Some(multiview_config::OutputOrientation::new(
            QuarterTurn::Cw90,
            OrientationMode::Tag,
            OutputFlip::None,
        )),
    };
    hls.validate().expect("HLS carries a display-matrix tag");
    assert_eq!(
        hls.orientation_tag_capability(),
        OrientationTagCapability::DisplayMatrix
    );
}

#[test]
fn auto_resolves_to_tag_on_hls_pixels_on_ts() {
    let o = multiview_config::OutputOrientation::new(
        QuarterTurn::Cw90,
        OrientationMode::Auto,
        OutputFlip::None,
    );
    assert_eq!(
        o.mechanism(OrientationTagCapability::DisplayMatrix),
        OrientationMechanism::Tag
    );
    assert_eq!(
        o.mechanism(OrientationTagCapability::None),
        OrientationMechanism::Pixels
    );
}

#[test]
fn flip_forces_pixels_even_in_auto_on_a_tag_transport() {
    let o = multiview_config::OutputOrientation::new(
        QuarterTurn::None,
        OrientationMode::Auto,
        OutputFlip::Vertical,
    );
    // Even though the transport can tag, a flip has no container tag ⇒ pixels.
    assert_eq!(
        o.mechanism(OrientationTagCapability::DisplayMatrix),
        OrientationMechanism::Pixels
    );
    assert!(o.has_flip());
    assert!(!o.is_identity());
}

#[test]
fn explicit_pixels_always_pixels() {
    let o = multiview_config::OutputOrientation::new(
        QuarterTurn::Cw180,
        OrientationMode::Pixels,
        OutputFlip::None,
    );
    assert_eq!(
        o.mechanism(OrientationTagCapability::DisplayMatrix),
        OrientationMechanism::Pixels
    );
}

#[test]
fn identity_orientation_is_identity() {
    let o = multiview_config::OutputOrientation::default();
    assert!(o.is_identity());
    assert!(!o.has_flip());
    assert_eq!(o.turn, QuarterTurn::None);
}
