//! Media-player & media-library config schema (ADR-0057 Decisions 1/2,
//! ADR-0097 vamp window, media-playout §3/§15) and the ADR-T015 §2 ms→frames
//! conversion.
//!
//! These exercise the **config serde mirror** of the media-player subsystem: the
//! declarative library/player blocks an operator authors, the additive
//! [`SourceKind`] bindings, and the document-level validation (vamp-window
//! nesting, id uniqueness, asset-ref resolution, the MVP player-count bound).
//! The runtime transport state machine lives in `multiview-cli`; this layer is
//! pure data + validation and must stay in the GPU-free CI baseline.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    frames_for_ms, EofPolicy, MediaAsset, MediaAssetKind, MultiviewConfig, SourceKind,
};
use multiview_core::time::Rational;

// ---------------------------------------------------------------------------
// ADR-T015 §2 — ms → frames (round-half-up, minimum 1 frame).
// frames(ms) = max(1, floor((2·ms·num + 1000·den) / (2000·den)))
// ---------------------------------------------------------------------------

#[test]
fn ms_to_frames_adr_t015_vectors() {
    // The four normative vectors from ADR-T015 §2 + the min-1 clamp row.
    assert_eq!(frames_for_ms(1000, Rational::new(60_000, 1001)), 60);
    assert_eq!(frames_for_ms(250, Rational::new(60_000, 1001)), 15);
    assert_eq!(frames_for_ms(1000, Rational::new(30_000, 1001)), 30);
    // 20ms @ 25fps = exactly 0.5 frames → half-up → 1.
    assert_eq!(frames_for_ms(20, Rational::new(25, 1)), 1);
    // 8ms @ 25fps = 0.2 frames → floor 0 → min-1 clamp → 1.
    assert_eq!(frames_for_ms(8, Rational::new(25, 1)), 1);
}

#[test]
fn ms_to_frames_min_one_clamp_never_zero() {
    // A requested duration never silently becomes a 0-frame no-op.
    assert_eq!(frames_for_ms(0, Rational::new(25, 1)), 1);
    assert_eq!(frames_for_ms(1, Rational::new(25, 1)), 1);
}

#[test]
fn ms_to_frames_exact_second_at_integer_rate() {
    assert_eq!(frames_for_ms(1000, Rational::new(25, 1)), 25);
    assert_eq!(frames_for_ms(1000, Rational::new(30, 1)), 30);
    assert_eq!(frames_for_ms(2000, Rational::new(60, 1)), 120);
}

#[test]
fn ms_to_frames_half_up_boundary() {
    // 100ms @ 25fps = exactly 2.5 frames → half-up → 3.
    assert_eq!(frames_for_ms(100, Rational::new(25, 1)), 3);
    // 60ms @ 25fps = 1.5 frames → half-up → 2.
    assert_eq!(frames_for_ms(60, Rational::new(25, 1)), 2);
}

#[test]
fn ms_to_frames_invalid_cadence_clamps_to_one() {
    // A degenerate cadence cannot define a frame period; the min-1 clamp keeps
    // the result usable rather than panicking or yielding 0.
    assert_eq!(frames_for_ms(1000, Rational::new(0, 1)), 1);
    assert_eq!(frames_for_ms(1000, Rational::new(30, 0)), 1);
}

// ---------------------------------------------------------------------------
// EofPolicy — config serde mirror of the cli runtime policy.
// ---------------------------------------------------------------------------

#[test]
fn eof_policy_default_is_hold_last_frame() {
    assert_eq!(EofPolicy::default(), EofPolicy::HoldLastFrame);
}

#[test]
fn eof_policy_serde_snake_case_round_trip() {
    for (policy, token) in [
        (EofPolicy::HoldLastFrame, "hold_last_frame"),
        (EofPolicy::Loop, "loop"),
        (EofPolicy::Black, "black"),
        (EofPolicy::AutoOff, "auto_off"),
    ] {
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(json, format!("\"{token}\""));
        let back: EofPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, policy);
    }
}

// ---------------------------------------------------------------------------
// MediaAsset / MediaAssetKind serde.
// ---------------------------------------------------------------------------

#[test]
fn media_asset_kind_serde_snake_case() {
    for (kind, token) in [
        (MediaAssetKind::Still, "still"),
        (MediaAssetKind::Clip, "clip"),
        (MediaAssetKind::Audio, "audio"),
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{token}\""));
        let back: MediaAssetKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }
}

#[test]
fn media_asset_minimal_round_trip() {
    let asset: MediaAsset = serde_json::from_value(serde_json::json!({
        "id": "opener",
        "kind": "clip",
        "path": "/media/opener.mov",
    }))
    .unwrap();
    assert_eq!(asset.id, "opener");
    assert_eq!(asset.kind, MediaAssetKind::Clip);
    assert_eq!(asset.path, "/media/opener.mov");
    // Optional declared fields default to absent / policy defaults.
    assert_eq!(asset.label, None);
    assert_eq!(asset.in_point_frames, None);
    assert_eq!(asset.out_point_frames, None);
    assert_eq!(asset.vamp_in_frames, None);
    assert_eq!(asset.vamp_out_frames, None);
    assert_eq!(asset.trigger_point_frames, None);
    assert_eq!(asset.default_eof_policy, EofPolicy::HoldLastFrame);
    assert!(!asset.default_loop);

    // Round-trips losslessly.
    let json = serde_json::to_string(&asset).unwrap();
    let back: MediaAsset = serde_json::from_str(&json).unwrap();
    assert_eq!(back, asset);
}

#[test]
fn media_asset_full_vamp_window_round_trip() {
    let asset: MediaAsset = serde_json::from_value(serde_json::json!({
        "id": "loop-bg",
        "label": "Background loop",
        "kind": "clip",
        "path": "/media/bg.mov",
        "in_point_frames": 10,
        "out_point_frames": 100,
        "vamp_in_frames": 20,
        "vamp_out_frames": 80,
        "trigger_point_frames": 50,
        "default_eof_policy": "loop",
        "default_loop": true,
    }))
    .unwrap();
    assert_eq!(asset.in_point_frames, Some(10));
    assert_eq!(asset.out_point_frames, Some(100));
    assert_eq!(asset.vamp_in_frames, Some(20));
    assert_eq!(asset.vamp_out_frames, Some(80));
    assert_eq!(asset.trigger_point_frames, Some(50));
    assert_eq!(asset.default_eof_policy, EofPolicy::Loop);
    assert!(asset.default_loop);

    let json = serde_json::to_string(&asset).unwrap();
    let back: MediaAsset = serde_json::from_str(&json).unwrap();
    assert_eq!(back, asset);
}

// ---------------------------------------------------------------------------
// MediaLibrary + MediaPlayer + the root media_players field serde.
// ---------------------------------------------------------------------------

#[test]
fn media_library_and_players_round_trip_in_document() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "root": "/srv/media",
            "assets": [
                { "id": "opener", "kind": "clip", "path": "opener.mov" },
                { "id": "still-1", "kind": "still", "path": "logo.png" }
            ]
        },
        "media_players": [
            { "id": "vt-1", "default": "opener" },
            { "id": "vt-2" }
        ]
    })))
    .unwrap();

    let library = doc.media_library.as_ref().expect("library present");
    assert_eq!(library.root.as_deref(), Some("/srv/media"));
    assert_eq!(library.assets.len(), 2);
    assert_eq!(library.assets[0].id, "opener");

    assert_eq!(doc.media_players.len(), 2);
    assert_eq!(doc.media_players[0].id, "vt-1");
    assert_eq!(doc.media_players[0].default.as_deref(), Some("opener"));
    assert_eq!(doc.media_players[1].default, None);
    // Player EOF defaults mirror the asset/runtime default.
    assert_eq!(doc.media_players[1].eof_policy, EofPolicy::HoldLastFrame);

    // The whole document round-trips losslessly through JSON.
    let json = serde_json::to_string(&doc).unwrap();
    let back: MultiviewConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.media_players, doc.media_players);
    assert_eq!(back.media_library, doc.media_library);
}

#[test]
fn media_players_default_empty_when_absent() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc()).unwrap();
    assert!(doc.media_players.is_empty());
    assert_eq!(doc.media_library, None);
}

// ---------------------------------------------------------------------------
// SourceKind additive variants — Still{asset} + MediaPlayer{player}.
// Distinct serde tags ⇒ both-populated is structurally impossible.
// ---------------------------------------------------------------------------

#[test]
fn source_kind_still_variant_round_trip() {
    let kind: SourceKind =
        serde_json::from_value(serde_json::json!({ "kind": "still", "asset": "logo" })).unwrap();
    assert!(matches!(&kind, SourceKind::Still { asset } if asset == "logo"));
    let json = serde_json::to_value(&kind).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "kind": "still", "asset": "logo" })
    );
}

#[test]
fn source_kind_media_player_variant_round_trip() {
    let kind: SourceKind =
        serde_json::from_value(serde_json::json!({ "kind": "media_player", "player": "vt-1" }))
            .unwrap();
    assert!(matches!(&kind, SourceKind::MediaPlayer { player } if player == "vt-1"));
    let json = serde_json::to_value(&kind).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "kind": "media_player", "player": "vt-1" })
    );
}

#[test]
fn source_kind_file_variant_untouched() {
    // The pre-existing File{path} variant is unchanged and distinct.
    let kind: SourceKind =
        serde_json::from_value(serde_json::json!({ "kind": "file", "path": "/m.mov" })).unwrap();
    assert!(matches!(&kind, SourceKind::File { path } if path == "/m.mov"));
}

// ---------------------------------------------------------------------------
// validate() — accept the happy path.
// ---------------------------------------------------------------------------

#[test]
fn validate_accepts_well_formed_library_and_players() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [
                {
                    "id": "opener",
                    "kind": "clip",
                    "path": "opener.mov",
                    "in_point_frames": 0,
                    "vamp_in_frames": 30,
                    "vamp_out_frames": 90,
                    "out_point_frames": 120
                }
            ]
        },
        "media_players": [
            { "id": "vt-1", "default": "opener" },
            { "id": "vt-2" }
        ]
    })))
    .unwrap();
    doc.validate().expect("well-formed media config validates");
}

// ---------------------------------------------------------------------------
// validate() — vamp-window nesting (in ≤ vamp_in < vamp_out ≤ out).
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_vamp_in_before_in_point() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{
                "id": "a", "kind": "clip", "path": "a.mov",
                "in_point_frames": 50, "vamp_in_frames": 20,
                "vamp_out_frames": 90, "out_point_frames": 120
            }]
        }
    })))
    .unwrap();
    let err = doc.validate().expect_err("vamp_in < in_point must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("vamp"),
        "error should name the vamp window: {msg}"
    );
}

#[test]
fn validate_rejects_vamp_out_after_out_point() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{
                "id": "a", "kind": "clip", "path": "a.mov",
                "in_point_frames": 0, "vamp_in_frames": 20,
                "vamp_out_frames": 200, "out_point_frames": 120
            }]
        }
    })))
    .unwrap();
    doc.validate().expect_err("vamp_out > out_point must fail");
}

#[test]
fn validate_rejects_degenerate_vamp_window() {
    // vamp_in == vamp_out is a zero-length vamp segment — degenerate, rejected.
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{
                "id": "a", "kind": "clip", "path": "a.mov",
                "in_point_frames": 0, "vamp_in_frames": 60,
                "vamp_out_frames": 60, "out_point_frames": 120
            }]
        }
    })))
    .unwrap();
    doc.validate()
        .expect_err("vamp_in == vamp_out (zero-length vamp) must fail");
}

#[test]
fn validate_rejects_out_before_in() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{
                "id": "a", "kind": "clip", "path": "a.mov",
                "in_point_frames": 120, "out_point_frames": 10
            }]
        }
    })))
    .unwrap();
    doc.validate().expect_err("out_point < in_point must fail");
}

#[test]
fn validate_accepts_partial_vamp_with_in_and_out_only() {
    // No vamp fields → only the in/out nesting is checked.
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{
                "id": "a", "kind": "clip", "path": "a.mov",
                "in_point_frames": 10, "out_point_frames": 120
            }]
        }
    })))
    .unwrap();
    doc.validate().expect("in/out-only window validates");
}

#[test]
fn validate_rejects_vamp_in_without_vamp_out() {
    // A half-specified vamp window is ambiguous and rejected.
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{
                "id": "a", "kind": "clip", "path": "a.mov",
                "in_point_frames": 0, "vamp_in_frames": 20, "out_point_frames": 120
            }]
        }
    })))
    .unwrap();
    doc.validate()
        .expect_err("vamp_in without vamp_out must fail");
}

// ---------------------------------------------------------------------------
// validate() — id uniqueness.
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_duplicate_asset_id() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [
                { "id": "dup", "kind": "clip", "path": "a.mov" },
                { "id": "dup", "kind": "still", "path": "b.png" }
            ]
        }
    })))
    .unwrap();
    let err = doc.validate().expect_err("duplicate asset id must fail");
    assert!(err.to_string().contains("dup"));
}

#[test]
fn validate_rejects_duplicate_player_id() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_players": [
            { "id": "vt-1" },
            { "id": "vt-1" }
        ]
    })))
    .unwrap();
    let err = doc.validate().expect_err("duplicate player id must fail");
    assert!(err.to_string().contains("vt-1"));
}

// ---------------------------------------------------------------------------
// validate() — asset-ref resolution.
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_player_default_unknown_asset() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{ "id": "opener", "kind": "clip", "path": "a.mov" }]
        },
        "media_players": [
            { "id": "vt-1", "default": "missing-asset" }
        ]
    })))
    .unwrap();
    let err = doc
        .validate()
        .expect_err("player default referencing unknown asset must fail");
    assert!(err.to_string().contains("missing-asset"));
}

#[test]
fn validate_rejects_still_source_unknown_asset() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "sources": [
            { "id": "logo-src", "kind": "still", "asset": "no-such-asset" }
        ]
    })))
    .unwrap();
    let err = doc
        .validate()
        .expect_err("still source referencing unknown asset must fail");
    assert!(err.to_string().contains("no-such-asset"));
}

#[test]
fn validate_rejects_media_player_source_unknown_player() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_players": [{ "id": "vt-1" }],
        "sources": [
            { "id": "vt-src", "kind": "media_player", "player": "vt-9" }
        ]
    })))
    .unwrap();
    let err = doc
        .validate()
        .expect_err("media_player source referencing unknown player must fail");
    assert!(err.to_string().contains("vt-9"));
}

#[test]
fn validate_accepts_resolving_source_refs() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_library": {
            "assets": [{ "id": "logo", "kind": "still", "path": "logo.png" }]
        },
        "media_players": [{ "id": "vt-1", "default": "logo" }],
        "sources": [
            { "id": "logo-src", "kind": "still", "asset": "logo" },
            { "id": "vt-src", "kind": "media_player", "player": "vt-1" }
        ]
    })))
    .unwrap();
    doc.validate().expect("resolving source refs validate");
}

// ---------------------------------------------------------------------------
// validate() — MVP player-count bound (≤ 2).
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_more_than_two_players() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_players": [
            { "id": "vt-1" },
            { "id": "vt-2" },
            { "id": "vt-3" }
        ]
    })))
    .unwrap();
    doc.validate()
        .expect_err("more than 2 media players exceeds the MVP bound");
}

#[test]
fn validate_accepts_exactly_two_players() {
    let doc: MultiviewConfig = serde_json::from_value(base_doc_with(serde_json::json!({
        "media_players": [
            { "id": "vt-1" },
            { "id": "vt-2" }
        ]
    })))
    .unwrap();
    doc.validate()
        .expect("exactly 2 players is within the MVP bound");
}

// ---------------------------------------------------------------------------
// Helpers — a minimal valid base document we splice media fields into.
// ---------------------------------------------------------------------------

/// A complete, otherwise-valid document: a 1-cell grid wired to a `bars`
/// source and a single output, so a full [`MultiviewConfig::validate`] run
/// reaches the media checks instead of tripping on an earlier invariant.
fn base_doc() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "canvas": {
            "width": 1920,
            "height": 1080,
            "fps": "25/1",
            "pixel_format": "nv12",
            "background": "#101014",
            "color": { "profile": "sdr-bt709-limited" }
        },
        "layout": { "kind": "grid", "columns": ["1fr"], "rows": ["1fr"], "areas": ["a"] },
        "sources": [{ "id": "in_a", "kind": "bars" }],
        "cells": [{ "id": "cell_a", "area": "a", "fit": "contain", "source": { "input_id": "in_a" } }],
        "outputs": [{ "kind": "rtsp_server", "mount": "/multiview", "codec": "h264" }]
    })
}

/// The base document with `extra` keys merged into the top-level object. Arrays
/// present in both the base and `extra` (e.g. `sources`) are **concatenated**
/// (base entries first) so a test that adds its own sources keeps the base's
/// `in_a` cell binding valid; non-array keys are overwritten.
fn base_doc_with(extra: serde_json::Value) -> serde_json::Value {
    let mut doc = base_doc();
    let (Some(obj), serde_json::Value::Object(extra)) = (doc.as_object_mut(), extra) else {
        panic!("base doc and extra must be JSON objects");
    };
    for (k, v) in extra {
        match (obj.get_mut(&k), v) {
            (Some(serde_json::Value::Array(base_arr)), serde_json::Value::Array(extra_arr)) => {
                base_arr.extend(extra_arr);
            }
            (_, v) => {
                obj.insert(k, v);
            }
        }
    }
    doc
}
