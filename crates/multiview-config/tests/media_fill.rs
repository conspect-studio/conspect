//! Declarative VT clip fills — the `[[media_fill]]` schema, its defaults, its
//! validation rules and its restart-class diff (ADR-0100 Decisions 1/3/7/10/12,
//! ADR-0101 Decisions 3/7).
//!
//! This layer is **pure data + validation** (no browser, no job runner, no
//! API): it proves a fill document round-trips, that the authored defaults
//! survive, and that every rule the ADRs demand is refused with a clear reason.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    CaptureMode, ConfigDiff, DuringRefresh, FillRefresh, FrameTiming, MediaFillKind,
    MultiviewConfig, ParamValue, SlotStorage, ViewBackground,
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

#[test]
fn defaults_are_applied_and_not_emitted() {
    let minimal = merge(
        web_fill(),
        serde_json::json!({
            "view": { "width": 1024, "height": 768 },
            "refresh": { "mode": "one_shot" }
        }),
    );
    let cfg = doc_with_fill(&minimal);
    cfg.validate().expect("minimal fill should validate");

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

    // A default-valued field is skipped on serialize (matching media.rs style).
    // Inspect the fill's own wire form so an unrelated document key (e.g. the
    // canvas `background`) cannot satisfy the assertion.
    let wire = serde_json::to_value(fill).expect("serialize fill");
    let mut keys: Vec<&str> = wire
        .as_object()
        .expect("fill is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["id", "kind", "player", "refresh", "template", "terms", "view"],
        "{wire}"
    );
    let view_wire = wire
        .get("view")
        .and_then(serde_json::Value::as_object)
        .expect("view is an object");
    let mut view_keys: Vec<&str> = view_wire.keys().map(String::as_str).collect();
    view_keys.sort_unstable();
    assert_eq!(view_keys, vec!["height", "width"], "{wire}");
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

#[test]
fn windy_requires_the_desktop_width() {
    let cfg = doc_with_fill(&with_view(
        web_fill(),
        &serde_json::json!({ "width": 960, "height": 540 }),
    ));
    let err = validation_error(&cfg);
    assert!(err.contains("windy"), "{err}");
    assert!(err.contains("1024"), "{err}");
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

// ---------------------------------------------------------------------------
// Diff: a fill change is part of the restart-class `media` section.
// ---------------------------------------------------------------------------

#[test]
fn a_fill_change_reports_the_media_section() {
    let doc = format!("{BASE_TOML}{FULL_FILLS_TOML}");
    let running = MultiviewConfig::load_from_toml(&doc).expect("running doc");
    let next = MultiviewConfig::load_from_toml(&doc.replace("minutes = 5", "minutes = 10"))
        .expect("next doc");

    let diff = ConfigDiff::between(&running, &next);
    assert!(
        diff.changed_sections.contains("media"),
        "a fill change must ride the media restart-class section: {:?}",
        diff.changed_sections
    );
    assert!(
        diff.changed_sections.len() == 1,
        "{:?}",
        diff.changed_sections
    );

    // An unchanged fill set is not a diff.
    let same = ConfigDiff::between(&running, &running);
    assert!(same.is_empty(), "{same:?}");
}
