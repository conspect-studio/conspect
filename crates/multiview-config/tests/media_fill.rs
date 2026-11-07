//! Declarative VT clip fills — the `[[media_fill]]` schema, its defaults, its
//! validation rules and its diff section (ADR-0100 Decisions 1/3/7/10/12,
//! ADR-0101 Decisions 1/3/7).
//!
//! This layer is **pure data + validation** (no browser, no job runner, no
//! API): it proves a fill document round-trips, that the authored defaults
//! survive, that every rule the ADRs demand is refused with a clear reason, and
//! that a fill edit is reported as live job intent (`media_fills`) rather than
//! as a restart-class media change.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chrono::{DateTime, Utc};
use multiview_config::{
    CaptureMode, ConfigDiff, DuringRefresh, FillRefresh, FrameTiming, MediaFillKind,
    MultiviewConfig, ParamValue, SlotStorage, TemplateConstraints, ViewBackground,
};

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A minimal valid TOML document with two media players and no fills.
const BASE_TOML: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "25/1"
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
kind = "bars"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"

[[media_players]]
id = "vt-1"

[[media_players]]
id = "vt-2"
"##;

/// The full worked example from ADR-0100 Decision 1 (`web_capture`) plus a
/// `url_fetch` fill on the second player.
const FULL_FILLS_TOML: &str = r#"
[[media_fill]]
id = "syd-radar"
player = "vt-1"
kind = "web_capture"
template = "windy"
refresh = { mode = "interval", minutes = 5 }
during_refresh = "na"
storage = "memory"
[media_fill.view]
width = 1920
height = 1080
capture = "continuous"
timing = "real"
frame_ms = 250
end_hold_ms = 1500
crossfade_ms = 1000
background = "opaque"
[media_fill.view.params]
overlay = "radar"
lat = -33.946
lon = 150.370
zoom = 10
hide_ui = true
[media_fill.terms]
user = "operator@example.com"
accepted_at = "2026-11-07T00:00:00Z"
template = "windy"
terms_version = "2023-09-01"

[[media_fill]]
id = "news-fetch"
player = "vt-2"
kind = "url_fetch"
url = "https://media.example.com/clip.mp4"
refresh = { mode = "one_shot" }
"#;

/// A minimal valid base document (JSON) to splice fill fields into.
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
        "outputs": [{ "kind": "rtsp_server", "mount": "/multiview", "codec": "h264" }],
        "media_players": [{ "id": "vt-1" }, { "id": "vt-2" }]
    })
}

/// Shallow-merge `overrides` into the top-level object of `base`.
fn merge(mut base: serde_json::Value, overrides: serde_json::Value) -> serde_json::Value {
    let (Some(obj), serde_json::Value::Object(extra)) = (base.as_object_mut(), overrides) else {
        panic!("base and overrides must be JSON objects");
    };
    for (k, v) in extra {
        obj.insert(k, v);
    }
    base
}

/// A valid `web_capture` fill, as JSON.
fn web_fill() -> serde_json::Value {
    serde_json::json!({
        "id": "syd-radar",
        "player": "vt-1",
        "kind": "web_capture",
        "template": "windy",
        "view": { "width": 1920, "height": 1080 },
        "refresh": { "mode": "interval", "minutes": 5 },
        "terms": {
            "user": "operator@example.com",
            "accepted_at": "2026-11-07T00:00:00Z",
            "template": "windy",
            "terms_version": "2023-09-01"
        }
    })
}

/// A valid `url_fetch` fill, as JSON.
fn url_fill(url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "news-fetch",
        "player": "vt-2",
        "kind": "url_fetch",
        "url": url,
        "refresh": { "mode": "one_shot" }
    })
}

/// Parse a document carrying `fills` (with the base's two players).
fn doc_with_fills(fills: &serde_json::Value) -> MultiviewConfig {
    let doc = merge(base_doc(), serde_json::json!({ "media_fill": fills }));
    MultiviewConfig::load_from_json(&doc.to_string()).expect("document should parse")
}

/// Parse a document carrying a single fill.
fn doc_with_fill(fill: &serde_json::Value) -> MultiviewConfig {
    doc_with_fills(&serde_json::Value::Array(vec![fill.clone()]))
}

/// Parse a document carrying `fills`, with `players` replacing the base's.
fn doc_with_players_and_fills(
    players: &serde_json::Value,
    fills: &serde_json::Value,
) -> MultiviewConfig {
    let doc = merge(
        base_doc(),
        serde_json::json!({ "media_players": players, "media_fill": fills }),
    );
    MultiviewConfig::load_from_json(&doc.to_string()).expect("document should parse")
}

/// The validation error text for `cfg`, or a panic if it unexpectedly passed.
fn validation_error(cfg: &MultiviewConfig) -> String {
    match cfg.validate() {
        Ok(()) => panic!("document unexpectedly validated"),
        Err(e) => e.to_string(),
    }
}

/// Replace a fill's `view` object entirely.
fn with_view(fill: serde_json::Value, view: &serde_json::Value) -> serde_json::Value {
    merge(fill, serde_json::json!({ "view": view }))
}

// ---------------------------------------------------------------------------
// Round trip + shape.
// ---------------------------------------------------------------------------

#[test]
fn full_fill_document_round_trips_through_toml_and_json() {
    let doc = format!("{BASE_TOML}{FULL_FILLS_TOML}");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("full example should parse");
    cfg.validate().expect("full example should validate");

    assert_eq!(cfg.media_fills.len(), 2);

    let radar = &cfg.media_fills[0];
    assert_eq!(radar.id, "syd-radar");
    assert_eq!(radar.player, "vt-1");
    assert_eq!(radar.during_refresh, DuringRefresh::Na);
    assert_eq!(radar.storage, SlotStorage::Memory);
    assert_eq!(radar.refresh, FillRefresh::Interval { minutes: 5 });
    let MediaFillKind::WebCapture { template, view } = &radar.kind else {
        panic!("expected a web_capture fill, got {:?}", radar.kind);
    };
    assert_eq!(template, "windy");
    assert_eq!(view.width, 1920);
    assert_eq!(view.height, 1080);
    assert_eq!(view.capture, CaptureMode::Continuous);
    assert_eq!(view.timing, FrameTiming::Real);
    assert_eq!(view.frame_ms, 250);
    assert_eq!(view.end_hold_ms, 1500);
    assert_eq!(view.crossfade_ms, 1000);
    assert_eq!(view.background, ViewBackground::Opaque);
    assert_eq!(
        view.params.get("overlay"),
        Some(&ParamValue::String("radar".into()))
    );
    assert_eq!(view.params.get("zoom"), Some(&ParamValue::Integer(10)));
    assert_eq!(view.params.get("lat"), Some(&ParamValue::Float(-33.946)));
    assert_eq!(view.params.get("hide_ui"), Some(&ParamValue::Bool(true)));
    let terms = radar.terms.as_ref().expect("terms present");
    assert_eq!(terms.template, "windy");
    assert_eq!(terms.terms_version, "2023-09-01");

    let news = &cfg.media_fills[1];
    assert_eq!(news.refresh, FillRefresh::OneShot);
    assert_eq!(news.during_refresh, DuringRefresh::PlayLast);
    assert_eq!(news.storage, SlotStorage::Disk);
    assert_eq!(
        news.kind,
        MediaFillKind::UrlFetch {
            url: "https://media.example.com/clip.mp4".to_owned()
        }
    );

    // TOML round trip.
    let toml_text = cfg.to_toml().expect("serialize to TOML");
    let back = MultiviewConfig::load_from_toml(&toml_text).expect("reparse TOML");
    assert_eq!(back, cfg);

    // JSON round trip (the canonical wire form).
    let json_text = cfg.to_json().expect("serialize to JSON");
    let back = MultiviewConfig::load_from_json(&json_text).expect("reparse JSON");
    assert_eq!(back, cfg);
}

/// Defaults are proven **behaviourally**, not by asserting a fixed serialized
/// key list (which breaks on any unrelated field addition): omitting a
/// default-valued field must produce exactly the same document as writing that
/// default out, and both forms must round-trip through JSON and TOML.
#[test]
fn omitted_defaults_equal_explicit_defaults_and_round_trip() {
    let lean = merge(
        web_fill(),
        serde_json::json!({
            "view": { "width": 1024, "height": 768 },
            "refresh": { "mode": "one_shot" }
        }),
    );
    let verbose = merge(
        web_fill(),
        serde_json::json!({
            "during_refresh": "play_last",
            "storage": "disk",
            "view": {
                "width": 1024, "height": 768,
                "capture": "steps", "timing": "equal",
                "frame_ms": 600, "end_hold_ms": 1000,
                "crossfade_ms": 0, "background": "opaque",
                "params": {}
            },
            "refresh": { "mode": "one_shot" }
        }),
    );
    let cfg = doc_with_fill(&lean);
    cfg.validate().expect("minimal fill should validate");
    let with_defaults = doc_with_fill(&verbose);
    with_defaults
        .validate()
        .expect("explicit-defaults fill should validate");

    // Omission ≡ explicit default (the authored wire says the same thing).
    assert_eq!(
        cfg, with_defaults,
        "omitting a default must equal writing it out"
    );

    // The defaults really were applied (spot the scalar-valued ones).
    let fill = &cfg.media_fills[0];
    assert_eq!(fill.during_refresh, DuringRefresh::PlayLast);
    assert_eq!(fill.storage, SlotStorage::Disk);
    let MediaFillKind::WebCapture { view, .. } = &fill.kind else {
        panic!("expected web_capture");
    };
    assert_eq!(view.capture, CaptureMode::Steps);
    assert_eq!(view.timing, FrameTiming::Equal);
    assert_eq!(view.frame_ms, 600);
    assert_eq!(view.end_hold_ms, 1000);
    assert_eq!(view.crossfade_ms, 0);
    assert_eq!(view.background, ViewBackground::Opaque);
    assert!(view.params.is_empty());

    // Both authored forms round-trip, and the round-trip stays equal to the
    // explicit-defaults document (so a dropped default would fail here).
    for text in [
        cfg.to_json().expect("serialize JSON"),
        cfg.to_toml().expect("serialize TOML"),
    ] {
        let back = if text.trim_start().starts_with('{') {
            MultiviewConfig::load_from_json(&text).expect("reparse JSON")
        } else {
            MultiviewConfig::load_from_toml(&text).expect("reparse TOML")
        };
        assert_eq!(back, cfg, "round trip changed the document");
        assert_eq!(back, with_defaults, "round trip lost a default");
    }
}

#[test]
fn config_without_fills_emits_no_media_fill_key() {
    let cfg = MultiviewConfig::load_from_toml(BASE_TOML).expect("base doc");
    assert_eq!(cfg.media_fills, []);
    let toml_text = cfg.to_toml().expect("serialize");
    assert!(
        !toml_text.contains("media_fill"),
        "an empty fill set must not serialize a key: {toml_text}"
    );
    // And the absent key deserializes as empty.
    let back = MultiviewConfig::load_from_toml(&toml_text).expect("reparse");
    assert_eq!(back, cfg);
}

#[test]
fn unknown_field_is_rejected_at_every_level() {
    // Fill level.
    let bad = merge(web_fill(), serde_json::json!({ "bogus": 1 }));
    let err = MultiviewConfig::load_from_json(
        &merge(base_doc(), serde_json::json!({"media_fill": [bad]})).to_string(),
    )
    .expect_err("an unknown fill key must be rejected");
    assert!(err.to_string().contains("bogus"), "{err}");

    // View level.
    let bad = with_view(
        web_fill(),
        &serde_json::json!({ "width": 1920, "height": 1080, "bogus": 1 }),
    );
    let err = MultiviewConfig::load_from_json(
        &merge(base_doc(), serde_json::json!({"media_fill": [bad]})).to_string(),
    )
    .expect_err("an unknown view key must be rejected");
    assert!(err.to_string().contains("bogus"), "{err}");

    // Terms level.
    let bad_terms = serde_json::json!({
        "user": "u", "accepted_at": "2026-11-07T00:00:00Z",
        "template": "windy", "terms_version": "v1", "bogus": 1
    });
    let bad = merge(web_fill(), serde_json::json!({ "terms": bad_terms }));
    let err = MultiviewConfig::load_from_json(
        &merge(base_doc(), serde_json::json!({"media_fill": [bad]})).to_string(),
    )
    .expect_err("an unknown terms key must be rejected");
    assert!(err.to_string().contains("bogus"), "{err}");
}

#[test]
fn kind_payload_mismatches_are_rejected() {
    // web_capture with a url.
    let bad = merge(
        web_fill(),
        serde_json::json!({ "url": "https://x.example/" }),
    );
    let err = MultiviewConfig::load_from_json(
        &merge(base_doc(), serde_json::json!({ "media_fill": [bad] })).to_string(),
    )
    .expect_err("web_capture + url must be rejected");
    assert!(err.to_string().contains("url_fetch"), "{err}");

    // url_fetch with a template/view.
    let bad = merge(
        url_fill("https://media.example.com/clip.mp4"),
        serde_json::json!({ "template": "windy" }),
    );
    let err = MultiviewConfig::load_from_json(
        &merge(base_doc(), serde_json::json!({ "media_fill": [bad] })).to_string(),
    )
    .expect_err("url_fetch + template must be rejected");
    assert!(err.to_string().contains("web_capture"), "{err}");

    // web_capture missing its view.
    let mut bad = web_fill();
    bad.as_object_mut().unwrap().remove("view");
    let err = MultiviewConfig::load_from_json(
        &merge(base_doc(), serde_json::json!({ "media_fill": [bad] })).to_string(),
    )
    .expect_err("web_capture without a view must be rejected");
    assert!(err.to_string().contains("view"), "{err}");
}

// ---------------------------------------------------------------------------
// Validation — happy paths.
// ---------------------------------------------------------------------------

#[test]
fn happy_path_web_capture_and_url_fetch_validate() {
    let cfg = doc_with_fills(&serde_json::json!([
        web_fill(),
        url_fill("https://media.example.com/clip.mp4")
    ]));
    cfg.validate().expect("the happy path must validate");
}

#[test]
fn globally_routable_ip_literal_hosts_are_accepted() {
    for url in [
        "https://[2001:db8::1]/clip.mp4",
        "https://93.184.216.34/clip.mp4",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        cfg.validate()
            .unwrap_or_else(|e| panic!("{url} should validate: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Validation — one failing case per rule.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_fill_ids_are_rejected() {
    let cfg = doc_with_players_and_fills(
        &serde_json::json!([{ "id": "vt-1" }, { "id": "vt-2" }]),
        &serde_json::json!([
            web_fill(),
            merge(
                url_fill("https://x.example/"),
                serde_json::json!({ "id": "syd-radar" })
            )
        ]),
    );
    let err = validation_error(&cfg);
    assert!(err.contains("duplicate media fill id"), "{err}");
}

#[test]
fn fill_referencing_an_unknown_player_is_rejected() {
    let cfg = doc_with_fill(&merge(web_fill(), serde_json::json!({ "player": "vt-9" })));
    let err = validation_error(&cfg);
    assert!(err.contains("unknown media player"), "{err}");
}

#[test]
fn fill_ids_must_be_1_to_64_id_characters() {
    let long_id = "a".repeat(65);
    for id in ["", " ", "has space", "bad!", "../escape", long_id.as_str()] {
        let cfg = doc_with_fill(&merge(web_fill(), serde_json::json!({ "id": id })));
        let err = validation_error(&cfg);
        assert!(err.contains("media fill id"), "{id:?}: {err}");
        assert!(err.contains("[A-Za-z0-9_-]"), "{id:?}: {err}");
    }
    // The full allowed alphabet, at the length boundary, is accepted.
    let id = format!("{}{}", "a".repeat(62), "Z9");
    doc_with_fill(&merge(web_fill(), serde_json::json!({ "id": id })))
        .validate()
        .expect("64 id characters including - and _ are accepted");
    doc_with_fill(&merge(web_fill(), serde_json::json!({ "id": "a-b_C9" })))
        .validate()
        .expect("hyphen and underscore ids are accepted");
}

#[test]
fn template_ids_must_be_1_to_64_id_characters() {
    let long_template = "t".repeat(65);
    for template in ["", "bad id", "bad!", long_template.as_str()] {
        let cfg = doc_with_fill(&merge(
            web_fill(),
            serde_json::json!({ "template": template }),
        ));
        let err = validation_error(&cfg);
        assert!(err.contains("template id"), "{template:?}: {err}");
        assert!(err.contains("[A-Za-z0-9_-]"), "{template:?}: {err}");
    }
    // Uppercase IS in the id alphabet, so a shape-valid id passes the id check
    // and fails later on the terms-template match instead (proving the id rule
    // is the documented alphabet, not a lowercase-only one).
    let err = validation_error(&doc_with_fill(&merge(
        web_fill(),
        serde_json::json!({ "template": "WINDY" }),
    )));
    assert!(err.contains("does not match"), "{err}");
}

#[test]
fn two_fills_on_one_player_are_rejected() {
    let cfg = doc_with_players_and_fills(
        &serde_json::json!([{ "id": "vt-1" }]),
        &serde_json::json!([
            web_fill(),
            merge(
                url_fill("https://x.example/"),
                serde_json::json!({ "player": "vt-1" })
            )
        ]),
    );
    let err = validation_error(&cfg);
    assert!(err.contains("at most one fill per player"), "{err}");
}

#[test]
fn interval_minutes_are_bounded() {
    for minutes in [0, 1441] {
        let cfg = doc_with_fill(&merge(
            web_fill(),
            serde_json::json!({ "refresh": { "mode": "interval", "minutes": minutes } }),
        ));
        let err = validation_error(&cfg);
        assert!(err.contains("out of range"), "{minutes}: {err}");
    }
    // The endpoints are allowed.
    for minutes in [1, 1440] {
        let cfg = doc_with_fill(&merge(
            web_fill(),
            serde_json::json!({ "refresh": { "mode": "interval", "minutes": minutes } }),
        ));
        cfg.validate()
            .unwrap_or_else(|e| panic!("{minutes} should validate: {e}"));
    }
}

#[test]
fn view_edges_must_be_even() {
    for view in [
        serde_json::json!({ "width": 1001, "height": 1080 }),
        serde_json::json!({ "width": 1920, "height": 1081 }),
    ] {
        let cfg = doc_with_fill(&with_view(web_fill(), &view));
        let err = validation_error(&cfg);
        assert!(err.contains("must be even"), "{err}");
    }
}

#[test]
fn view_edges_are_bounded() {
    for view in [
        serde_json::json!({ "width": 62, "height": 1080 }),
        serde_json::json!({ "width": 7682, "height": 1080 }),
        serde_json::json!({ "width": 1920, "height": 62 }),
        serde_json::json!({ "width": 1920, "height": 4322 }),
    ] {
        let cfg = doc_with_fill(&with_view(web_fill(), &view));
        let err = validation_error(&cfg);
        assert!(err.contains("out of range"), "{err}");
    }
    // The endpoints are allowed for a non-windy template.
    let cfg = doc_with_fill(&merge(
        web_fill(),
        serde_json::json!({
            "template": "bom",
            "view": { "width": 64, "height": 64 },
            "terms": {
                "user": "u", "accepted_at": "2026-11-07T00:00:00Z",
                "template": "bom", "terms_version": "v1"
            }
        }),
    ));
    cfg.validate().expect("endpoint geometry should validate");
}

/// A fake template registry: `windy` declares ADR-0101 Decision 3's desktop
/// floor, every other template declares none. The generic config must learn
/// this only through the injected hook — it holds no site knowledge.
struct FakeTemplates;

impl TemplateConstraints for FakeTemplates {
    fn min_width(&self, template: &str) -> Option<u32> {
        (template == "windy").then_some(1024)
    }
}

#[test]
fn template_width_floor_comes_only_from_the_injected_constraints() {
    let cfg = doc_with_fill(&with_view(
        web_fill(),
        &serde_json::json!({ "width": 960, "height": 540 }),
    ));

    // Without constraints the generic config applies no per-template floor: the
    // hard-coded windy rule is gone from the schema.
    cfg.validate()
        .expect("the schema itself carries no per-template width rule");

    // With the template registry injected, the declared floor is enforced.
    let err = cfg
        .validate_with(Some(&FakeTemplates), None)
        .expect_err("the declared floor must be enforced")
        .to_string();
    assert!(err.contains("template constraint"), "{err}");
    assert!(err.contains("1024"), "{err}");

    // At the floor it passes.
    let wide = doc_with_fill(&with_view(
        web_fill(),
        &serde_json::json!({ "width": 1024, "height": 576 }),
    ));
    wide.validate_with(Some(&FakeTemplates), None)
        .expect("a width at the floor passes");

    // A template that declares no floor is unconstrained by the hook.
    let other = doc_with_fill(&merge(
        web_fill(),
        serde_json::json!({
            "template": "bom",
            "view": { "width": 960, "height": 540 },
            "terms": {
                "user": "u", "accepted_at": "2026-11-07T00:00:00Z",
                "template": "bom", "terms_version": "v1"
            }
        }),
    ));
    other
        .validate_with(Some(&FakeTemplates), None)
        .expect("a template with no floor passes");
}

#[test]
fn frame_ms_is_bounded() {
    for frame_ms in [39, 60_001] {
        let cfg = doc_with_fill(&with_view(
            web_fill(),
            &serde_json::json!({ "width": 1920, "height": 1080, "frame_ms": frame_ms }),
        ));
        let err = validation_error(&cfg);
        assert!(err.contains("frame_ms"), "{frame_ms}: {err}");
    }
}

#[test]
fn hold_and_crossfade_are_bounded() {
    for view in [
        serde_json::json!({ "width": 1920, "height": 1080, "end_hold_ms": 60_001 }),
        serde_json::json!({ "width": 1920, "height": 1080, "crossfade_ms": 60_001 }),
    ] {
        let cfg = doc_with_fill(&with_view(web_fill(), &view));
        let err = validation_error(&cfg);
        assert!(err.contains("exceeds 60000"), "{err}");
    }
    // 0 holds are fine, and the cap is inclusive.
    let cfg = doc_with_fill(&with_view(
        web_fill(),
        &serde_json::json!({
            "width": 1920, "height": 1080,
            "end_hold_ms": 60_000, "crossfade_ms": 0
        }),
    ));
    cfg.validate().expect("boundary durations should validate");
}

#[test]
fn transparent_background_is_rejected_until_alpha_lands() {
    let cfg = doc_with_fill(&with_view(
        web_fill(),
        &serde_json::json!({ "width": 1920, "height": 1080, "background": "transparent" }),
    ));
    let err = validation_error(&cfg);
    assert!(err.contains("transparent"), "{err}");
    assert!(err.contains("ADR-0100 step 7"), "{err}");
}

#[test]
fn web_capture_requires_terms_acceptance() {
    let mut no_terms = web_fill();
    no_terms.as_object_mut().unwrap().remove("terms");
    let cfg = doc_with_fill(&no_terms);
    let err = validation_error(&cfg);
    assert!(err.contains("terms"), "{err}");
    assert!(err.contains("ADR-0101"), "{err}");
}

#[test]
fn terms_template_must_match_the_fill_template() {
    let bad_terms = serde_json::json!({
        "user": "u", "accepted_at": "2026-11-07T00:00:00Z",
        "template": "other", "terms_version": "v1"
    });
    let cfg = doc_with_fill(&merge(
        web_fill(),
        serde_json::json!({ "terms": bad_terms }),
    ));
    let err = validation_error(&cfg);
    assert!(err.contains("does not match"), "{err}");
}

#[test]
fn terms_accepted_at_must_be_rfc3339() {
    let bad_terms = serde_json::json!({
        "user": "u", "accepted_at": "yesterday",
        "template": "windy", "terms_version": "v1"
    });
    let cfg = doc_with_fill(&merge(
        web_fill(),
        serde_json::json!({ "terms": bad_terms }),
    ));
    let err = validation_error(&cfg);
    assert!(err.contains("RFC 3339"), "{err}");
}

/// Build a `web_capture` fill whose terms block is `user`/`terms_version` as
/// given (RFC 3339 stamp and matching template otherwise valid).
fn web_fill_with_terms(user: &str, terms_version: &str) -> serde_json::Value {
    merge(
        web_fill(),
        serde_json::json!({
            "terms": {
                "user": user,
                "accepted_at": "2026-11-07T00:00:00Z",
                "template": "windy",
                "terms_version": terms_version
            }
        }),
    )
}

#[test]
fn terms_user_must_be_non_blank_trimmed_bounded_and_control_free() {
    // Blank / whitespace-only.
    for user in ["", "   ", "\t"] {
        let err = validation_error(&doc_with_fill(&web_fill_with_terms(user, "v1")));
        assert!(err.contains("terms.user"), "{user:?}: {err}");
    }
    // Padded: the API stamps this value, so it must already be trimmed.
    let err = validation_error(&doc_with_fill(&web_fill_with_terms(
        " operator@example.com ",
        "v1",
    )));
    assert!(err.contains("trimmed"), "{err}");
    // Over 256 characters.
    let err = validation_error(&doc_with_fill(&web_fill_with_terms(&"u".repeat(257), "v1")));
    assert!(err.contains("terms.user"), "{err}");
    assert!(err.contains("256"), "{err}");
    // A control character.
    let err = validation_error(&doc_with_fill(&web_fill_with_terms("oper\u{7f}ator", "v1")));
    assert!(err.contains("control character"), "{err}");
    // The boundary length is accepted.
    doc_with_fill(&web_fill_with_terms(&"u".repeat(256), "v1"))
        .validate()
        .expect("256 characters is within the bound");
}

#[test]
fn terms_version_must_be_non_blank_trimmed_bounded_and_control_free() {
    for version in ["", "  "] {
        let err = validation_error(&doc_with_fill(&web_fill_with_terms("u", version)));
        assert!(err.contains("terms.terms_version"), "{version:?}: {err}");
    }
    let err = validation_error(&doc_with_fill(&web_fill_with_terms("u", " v1 ")));
    assert!(err.contains("trimmed"), "{err}");
    let err = validation_error(&doc_with_fill(&web_fill_with_terms("u", &"v".repeat(65))));
    assert!(err.contains("terms.terms_version"), "{err}");
    assert!(err.contains("64"), "{err}");
    let err = validation_error(&doc_with_fill(&web_fill_with_terms("u", "v1\u{1}")));
    assert!(err.contains("control character"), "{err}");
    doc_with_fill(&web_fill_with_terms("u", &"v".repeat(64)))
        .validate()
        .expect("64 characters is within the bound");
}

#[test]
fn terms_accepted_at_may_not_lead_an_injected_clock() {
    let cfg = doc_with_fill(&web_fill_with_terms("u", "v1"));
    let now = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .expect("test clock parses")
            .with_timezone(&Utc)
    };

    // The stamp is 10 minutes ahead of the injected clock: forged / mis-stamped.
    let err = cfg
        .validate_with_now(now("2026-11-06T23:50:00Z"))
        .expect_err("a 10-minute future stamp must be rejected")
        .to_string();
    assert!(err.contains("future"), "{err}");
    // The fill-level seam rejects it too.
    assert!(cfg.media_fills[0]
        .validate_with_now(now("2026-11-06T23:50:00Z"))
        .is_err());

    // Exactly at the 5-minute allowance and inside it: accepted.
    cfg.validate_with_now(now("2026-11-06T23:55:00Z"))
        .expect("exactly 5 minutes of skew is allowed");
    cfg.validate_with_now(now("2026-11-06T23:57:00Z"))
        .expect("3 minutes of skew is allowed");

    // The normal `validate` has no clock at all, so the future check is skipped
    // (documented: the API stamps and the runner enforces the version match).
    cfg.validate()
        .expect("without a clock the future check cannot run");
}

#[test]
fn url_fetch_requires_a_parseable_https_url() {
    for url in [
        "notaurl",
        "http://media.example.com/clip.mp4",
        "ftp://x.example/f",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        let err = validation_error(&cfg);
        assert!(
            err.contains("https") || err.contains("not a URL"),
            "{url}: {err}"
        );
    }
}

#[test]
fn url_fetch_rejects_userinfo() {
    let cfg = doc_with_fill(&url_fill("https://user:pass@media.example.com/clip.mp4"));
    let err = validation_error(&cfg);
    assert!(err.contains("userinfo"), "{err}");
}

#[test]
fn url_fetch_rejects_non_public_ip_literal_hosts() {
    for url in [
        "https://127.0.0.1/clip.mp4",
        "https://192.168.1.10/clip.mp4",
        "https://10.0.0.5/clip.mp4",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/clip.mp4",
        "https://[fd00::1]/clip.mp4",
        "https://[fe80::1]/clip.mp4",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        let err = validation_error(&cfg);
        assert!(err.contains("SSRF guard"), "{url}: {err}");
    }
}

#[test]
fn url_fetch_normalises_ipv4_notations_before_screening() {
    // The WHATWG parser normalises decimal and hex IPv4 to a real `Ipv4` host;
    // the screen therefore sees 127.0.0.1 even though the text is not an IP.
    for url in ["https://2130706433/", "https://0x7f.1/"] {
        let cfg = doc_with_fill(&url_fill(url));
        let err = validation_error(&cfg);
        assert!(err.contains("SSRF guard"), "{url}: {err}");
        assert!(err.contains("127.0.0.1"), "{url}: {err}");
    }
}

/// A `url_fetch` URL is stored in the parser's **canonical** form, not the
/// operator's spelling: the fill carries exactly what the WHATWG parser
/// interpreted, and that canonical form re-parses to itself, so
/// serialize → deserialize is stable.
#[test]
fn url_fetch_stores_the_canonical_parsed_form() {
    let cfg = doc_with_fill(&url_fill("https://Example.COM/a"));
    cfg.validate().expect("a normal https URL validates");
    assert_eq!(
        cfg.media_fills[0].kind,
        MediaFillKind::UrlFetch {
            url: "https://example.com/a".to_owned()
        },
        "the stored URL must be the parser's canonical rendering"
    );

    // serialize → deserialize is stable: a reloaded document is identical to
    // the one that was serialized (a canonical URL re-parses to itself).
    for text in [
        cfg.to_json().expect("serialize JSON"),
        cfg.to_toml().expect("serialize TOML"),
    ] {
        let back = if text.trim_start().starts_with('{') {
            MultiviewConfig::load_from_json(&text).expect("reparse JSON")
        } else {
            MultiviewConfig::load_from_toml(&text).expect("reparse TOML")
        };
        assert_eq!(back, cfg, "round trip changed the document");
    }
}

/// The trailing-root-dot IPv4 notations are normalised by the parser to the
/// `127.0.0.1` they denote, stored as that canonical literal, and then refused
/// by the SSRF screen — the dot must not smuggle loopback past the guard.
#[test]
fn url_fetch_rejects_trailing_dot_ipv4_notations() {
    for url in [
        "https://127.0.0.1./",
        "https://2130706433./",
        "https://0x7f.1./",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        let err = validation_error(&cfg);
        assert!(err.contains("SSRF guard"), "{url}: {err}");
        assert!(err.contains("127.0.0.1"), "{url}: {err}");
    }
}

#[test]
fn url_fetch_rejects_local_and_special_use_hosts() {
    for url in [
        "https://localhost/clip.mp4",
        "https://localhost./clip.mp4",
        "https://foo.localhost/clip.mp4",
        "https://printer.local/clip.mp4",
        "https://svc.internal/clip.mp4",
        "https://a.home.arpa/clip.mp4",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        let err = validation_error(&cfg);
        assert!(
            err.contains("local/special-use"),
            "{url} must be refused as a local name: {err}"
        );
    }
}

#[test]
fn url_fetch_accepts_and_normalises_idn_hosts() {
    // A Unicode IDN name is punycoded by the parser and accepted...
    for url in [
        "https://bücher.example/clip.mp4",
        "https://xn--bcher-kva.example/clip.mp4",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        cfg.validate()
            .unwrap_or_else(|e| panic!("{url} should validate: {e}"));
    }
}

#[test]
fn url_fetch_is_bounded_to_2048_bytes() {
    let base = "https://media.example.com/";
    let at_limit = format!("{base}{}", "a".repeat(2048 - base.len()));
    assert_eq!(at_limit.len(), 2048);
    doc_with_fill(&url_fill(&at_limit))
        .validate()
        .expect("a 2048-byte URL is accepted");
    let too_long = format!("{at_limit}a");
    let err = validation_error(&doc_with_fill(&url_fill(&too_long)));
    assert!(err.contains("2048"), "{err}");
}

#[test]
fn url_fetch_rejects_raw_whitespace_and_control_characters() {
    for url in [
        "https://invalid host/clip.mp4",
        "https://exam\nple.com/clip.mp4",
        "https://example.com/\u{7f}",
    ] {
        let cfg = doc_with_fill(&url_fill(url));
        let err = validation_error(&cfg);
        assert!(
            err.contains("whitespace") || err.contains("control"),
            "{url:?}: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// Validation — template parameters (ADR-0101 Decision 1).
// ---------------------------------------------------------------------------

/// A one-entry parameter map.
fn one_param(key: &str, value: serde_json::Value) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(key.to_owned(), value);
    serde_json::Value::Object(map)
}

/// A fill whose view carries `params`.
fn fill_with_params(params: &serde_json::Value) -> MultiviewConfig {
    doc_with_fill(&with_view(
        web_fill(),
        &serde_json::json!({ "width": 1920, "height": 1080, "params": params }),
    ))
}

#[test]
fn toml_non_finite_param_floats_are_rejected_at_parse_time() {
    const PARAM_FILL: &str = r#"
[[media_fill]]
id = "p1"
player = "vt-1"
kind = "web_capture"
template = "windy"
refresh = { mode = "one_shot" }
[media_fill.view]
width = 1920
height = 1080
[media_fill.view.params]
lat = LITERAL
[media_fill.terms]
user = "u"
accepted_at = "2026-11-07T00:00:00Z"
template = "windy"
terms_version = "v1"
"#;
    for literal in ["nan", "inf", "-inf"] {
        let doc = format!("{BASE_TOML}{}", PARAM_FILL.replace("LITERAL", literal));
        let err = MultiviewConfig::load_from_toml(&doc)
            .expect_err("a non-finite float must fail to parse")
            .to_string();
        assert!(err.contains("not finite"), "{literal}: {err}");
    }
    // A finite float still parses and validates.
    let doc = format!("{BASE_TOML}{}", PARAM_FILL.replace("LITERAL", "-33.946"));
    MultiviewConfig::load_from_toml(&doc)
        .expect("a finite float parses")
        .validate()
        .expect("a finite float validates");
}

#[test]
fn json_integers_above_i64_max_are_rejected_at_parse_time() {
    let with_big = |number: serde_json::Value| {
        merge(
            base_doc(),
            serde_json::json!({
                "media_fill": [with_view(
                    web_fill(),
                    &serde_json::json!({
                        "width": 1920, "height": 1080,
                        "params": { "big": number }
                    })
                )]
            }),
        )
    };
    let err = MultiviewConfig::load_from_json(
        &with_big(serde_json::json!(9_223_372_036_854_775_808u64)).to_string(),
    )
    .expect_err("a u64 above i64::MAX must be an error, not a float")
    .to_string();
    assert!(err.contains("i64"), "{err}");
    // `i64::MAX` itself is accepted.
    MultiviewConfig::load_from_json(&with_big(serde_json::json!(i64::MAX)).to_string())
        .expect("i64::MAX parses")
        .validate()
        .expect("i64::MAX validates");
}

#[test]
fn param_maps_are_bounded_shaped_and_control_free() {
    // At most 32 entries.
    let mut many = serde_json::Map::new();
    for index in 0..33 {
        many.insert(format!("p{index}"), serde_json::json!(index));
    }
    let err = validation_error(&fill_with_params(&serde_json::Value::Object(many)));
    assert!(err.contains("exceeds the bound"), "{err}");

    // Keys are 1..=64 characters of [a-z0-9_].
    let long_key = "k".repeat(65);
    for key in ["", "Zoom", "has space", "dash-key", long_key.as_str()] {
        let err = validation_error(&fill_with_params(&one_param(key, serde_json::json!(1))));
        assert!(err.contains("template parameter key"), "{key:?}: {err}");
    }

    // String values: at most 1024 characters, no control characters.
    let err = validation_error(&fill_with_params(&one_param(
        "note",
        serde_json::json!("x".repeat(1025)),
    )));
    assert!(err.contains("1024"), "{err}");
    let err = validation_error(&fill_with_params(&one_param(
        "note",
        serde_json::json!("a\u{7}b"),
    )));
    assert!(err.contains("control character"), "{err}");

    // Valid shapes: bool, integer, float, a bounded string and every key class.
    fill_with_params(&serde_json::json!({
        "hide_ui": true, "zoom": 10, "lat": -33.946, "note_2": "x".repeat(1024)
    }))
    .validate()
    .expect("valid parameter shapes must validate");
}

// ---------------------------------------------------------------------------
// Diff: a fill edit is live job intent (`media_fills`), a player/library edit
// stays restart-class (`media`). `ConfigDiff` has no per-section
// "requires restart" flag — `changed_sections` is the reporting surface the
// apply layer classifies — so the section membership IS the assertion.
// ---------------------------------------------------------------------------

#[test]
fn a_fill_change_reports_the_live_media_fills_section_and_not_media() {
    let doc = format!("{BASE_TOML}{FULL_FILLS_TOML}");
    let running = MultiviewConfig::load_from_toml(&doc).expect("running doc");
    let next = MultiviewConfig::load_from_toml(&doc.replace("minutes = 5", "minutes = 10"))
        .expect("next doc");

    let diff = ConfigDiff::between(&running, &next);
    assert!(
        diff.changed_sections.contains("media_fills"),
        "a fill change must be reported as live job intent: {:?}",
        diff.changed_sections
    );
    assert!(
        !diff.changed_sections.contains("media"),
        "a fill edit must NOT be restart-class media: {:?}",
        diff.changed_sections
    );
    assert_eq!(
        diff.changed_sections.iter().copied().collect::<Vec<_>>(),
        vec!["media_fills"],
        "a lone fill edit is exactly one section"
    );

    // An unchanged fill set is not a diff.
    let same = ConfigDiff::between(&running, &running);
    assert!(same.is_empty(), "{same:?}");
}

#[test]
fn a_player_change_still_reports_the_restart_class_media_section() {
    let running = MultiviewConfig::load_from_toml(BASE_TOML).expect("running doc");
    // Rename a declared player: the player set is restart-class, not job intent.
    let next =
        MultiviewConfig::load_from_toml(&BASE_TOML.replace("vt-2", "vt-3")).expect("next doc");

    let diff = ConfigDiff::between(&running, &next);
    assert!(
        diff.changed_sections.contains("media"),
        "a player change is restart-class media: {:?}",
        diff.changed_sections
    );
    assert!(
        !diff.changed_sections.contains("media_fills"),
        "no fill changed, so media_fills must not fire: {:?}",
        diff.changed_sections
    );

    // A fill edit and a player edit together report both sections (the renamed
    // player id is replaced consistently in the player declaration and the
    // fill that binds it, so only the two independent deltas remain).
    let doc = format!("{BASE_TOML}{FULL_FILLS_TOML}");
    let both_running = MultiviewConfig::load_from_toml(&doc).expect("running doc");
    let both_next = MultiviewConfig::load_from_toml(
        &doc.replace("minutes = 5", "minutes = 10")
            .replace("vt-2", "vt-3"),
    )
    .expect("changed doc");
    let both = ConfigDiff::between(&both_running, &both_next);
    assert!(both.changed_sections.contains("media"), "{both:?}");
    assert!(both.changed_sections.contains("media_fills"), "{both:?}");
}
