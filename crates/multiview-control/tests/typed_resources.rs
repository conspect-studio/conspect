//! Typed resource validation (ADR-W015): source/output/overlay bodies must
//! deserialize against the canonical `multiview_config` types at the API
//! boundary — invalid documents are rejected with `422 /problems/validation`
//! carrying the offending field path, and valid mutations declare their apply
//! semantics via the `X-Multiview-Apply` header.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{body_json, harness, post_json, put_json, send, OPERATOR_TOKEN};

const APPLY_HEADER: &str = "x-multiview-apply";

#[tokio::test]
async fn create_source_with_unknown_kind_is_422_with_field_detail() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "flux-capacitor" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    let detail = problem["detail"].as_str().expect("detail is present");
    assert!(
        detail.contains("flux-capacitor") || detail.contains("kind"),
        "detail names the offending field/variant, got: {detail}"
    );
}

#[tokio::test]
async fn create_source_missing_required_field_is_422() {
    let h = harness();
    // An rtsp source without its `url` must be rejected.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "rtsp" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("url"),
        "detail names the missing field, got: {}",
        problem["detail"]
    );
}

#[tokio::test]
async fn create_source_with_mismatched_body_id_is_422() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "other", "kind": "bars" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn create_source_without_body_id_inherits_the_path_id() {
    let h = harness();
    // The body `id` may be omitted; the resource id from the path is used.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "kind": "bars" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(created["body"]["id"], "cam1", "the path id is injected");
}

#[tokio::test]
async fn valid_source_mutations_declare_restart_apply_semantics() {
    let h = harness();
    let body = json!({
        "name": "Cam 1",
        "body": { "id": "cam1", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam1" }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/cam1", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()
            .get(APPLY_HEADER)
            .expect("create declares apply semantics")
            .to_str()
            .unwrap(),
        "restart"
    );

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &body,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(APPLY_HEADER)
            .expect("update declares apply semantics")
            .to_str()
            .unwrap(),
        "restart"
    );
}

#[tokio::test]
async fn create_output_missing_required_field_is_422() {
    let h = harness();
    // An rtmp output without its destination `url` must be rejected.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/push1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Push 1",
                "body": { "id": "push1", "kind": "rtmp" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn create_valid_ll_hls_output_succeeds_with_apply_header() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/web1",
            OPERATOR_TOKEN,
            &json!({
                "name": "LL-HLS",
                "body": { "id": "web1", "kind": "ll_hls", "path": "/var/lib/multiview/hls", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get(APPLY_HEADER).unwrap().to_str().unwrap(),
        "restart"
    );
}

#[tokio::test]
async fn create_overlay_with_invalid_shape_is_422() {
    let h = harness();
    // An overlay body must at least be an object with a string `kind`.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clock1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Clock",
                "body": { "id": "clock1", "kind": 7 }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn create_valid_overlay_succeeds() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clock1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Clock",
                "body": { "id": "clock1", "kind": "clock", "target": "canvas" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// Pin the `OpenAPI` mirror schemas (`openapi_schemas`) to the real
/// `multiview_config` types: every representative document must be accepted by
/// BOTH (a mirror that drifts fails here, per ADR-W015).
#[test]
fn openapi_mirrors_accept_what_the_config_types_accept() {
    let sources = [
        json!({ "id": "s", "kind": "bars" }),
        json!({ "id": "s", "kind": "solid", "color": "#101014" }),
        json!({ "id": "s", "kind": "clock", "face": "digital", "twelve_hour": true, "tz_offset_minutes": 600 }),
        json!({ "id": "s", "kind": "timer", "target": "time_of_day", "at": "14:30:00",
                "timezone": "Australia/Sydney", "recur_daily": true, "direction": "down",
                "on_target": "zero_then_up", "format": "auto", "label": "ON AIR IN" }),
        json!({ "id": "s", "kind": "timer", "target": "date_time", "at": "2026-07-01T09:00:00",
                "timezone": "UTC", "format": "d_hh_mm_ss" }),
        json!({ "id": "s", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam", "rtsp": { "transport": "tcp" } }),
        json!({ "id": "s", "kind": "hls", "url": "https://example.com/x.m3u8" }),
        json!({ "id": "s", "kind": "youtube", "url": "https://www.youtube.com/watch?v=abc" }),
        json!({ "id": "s", "kind": "ts", "url": "udp://[ff3e::1]:5000" }),
        json!({ "id": "s", "kind": "srt", "url": "srt://[2001:db8::2]:9000" }),
        json!({ "id": "s", "kind": "rtmp", "url": "rtmp://example.com/live/x" }),
        json!({ "id": "s", "kind": "ndi", "name": "STUDIO (CAM 1)" }),
        json!({ "id": "s", "kind": "file", "path": "/media/loop.ts" }),
        json!({ "id": "s", "kind": "aes67", "sdp": "v=0\r\n...", "multicast": "[ff3e::1]:5004", "ptp_domain": 0 }),
        json!({
            "id": "s", "kind": "rtsp", "url": "rtsp://h/x",
            "display_name": "Cam",
            "auth": { "secret_ref": "op://Servers/cam/credentials" },
            "color_override": { "primaries": "bt709" },
            "captions": { "mode": "teletext_page", "page": 801 },
            "gpu_pin": { "vendor": "nvidia", "stable_id": "GPU-uuid" },
            "wallclock": { "use": "discard" }
        }),
    ];
    for doc in &sources {
        let real: Result<multiview_config::Source, _> = serde_json::from_value(doc.clone());
        let mirror: Result<multiview_control::openapi_schemas::SourceBodyDoc, _> =
            serde_json::from_value(doc.clone());
        assert!(real.is_ok(), "config rejects {doc}: {:?}", real.err());
        assert!(mirror.is_ok(), "mirror rejects {doc}: {:?}", mirror.err());
    }

    let outputs = [
        json!({ "kind": "rtsp_server", "mount": "/mv", "codec": "h264", "latency_profile": "low" }),
        json!({ "kind": "ll_hls", "path": "/srv/hls", "codec": "h264", "part_target_ms": 333, "segment_ms": 2000, "gop_ms": 1000 }),
        json!({ "kind": "hls", "path": "/srv/hls", "codec": "hevc", "segment_ms": 4000 }),
        json!({ "kind": "ndi", "name": "MULTIVIEW" }),
        json!({ "kind": "rtmp", "url": "rtmp://ingest.example/live/k", "codec": "h264" }),
        json!({ "kind": "srt", "url": "srt://[2001:db8::3]:7000", "codec": "h264",
                "gpu_pin": { "vendor": "intel", "stable_id": "pci-0000:00:02.0" },
                "audio": { "mode": "program" } }),
        json!({ "kind": "aes67", "label": "PGM AES67", "multicast": "[ff3e::1]:5004",
                "depth": "L24", "ptime_ms": 1 }),
    ];
    for doc in &outputs {
        let real: Result<multiview_config::Output, _> = serde_json::from_value(doc.clone());
        let mirror: Result<multiview_control::openapi_schemas::OutputBodyDoc, _> =
            serde_json::from_value(doc.clone());
        assert!(real.is_ok(), "config rejects {doc}: {:?}", real.err());
        assert!(mirror.is_ok(), "mirror rejects {doc}: {:?}", mirror.err());
    }

    let overlay =
        json!({ "id": "o", "kind": "clock", "target": "canvas", "z": 5, "face": "analog" });
    assert!(serde_json::from_value::<multiview_config::Overlay>(overlay.clone()).is_ok());
    assert!(
        serde_json::from_value::<multiview_control::openapi_schemas::OverlayBodyDoc>(overlay)
            .is_ok()
    );
}

#[tokio::test]
async fn semantically_invalid_bodies_are_422_even_when_well_typed() {
    let h = harness();
    // Well-typed but semantically wrong documents must be rejected at the API
    // boundary (review M3) — they would otherwise poison /config/export.
    let cases = [
        (
            "/api/v1/sources/s1",
            json!({ "name": "S", "body": { "kind": "solid", "color": "chartreuse" } }),
        ),
        (
            "/api/v1/sources/s2",
            json!({ "name": "S", "body": { "kind": "clock", "tz_offset_minutes": 99999 } }),
        ),
        (
            "/api/v1/outputs/o1",
            json!({ "name": "O", "body": { "kind": "rtmp", "url": "rtmp://h/x", "codec": "" } }),
        ),
    ];
    for (path, body) in &cases {
        let resp = send(&h.router, post_json(path, OPERATOR_TOKEN, body)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path} must reject a semantically invalid body"
        );
    }
}

#[tokio::test]
async fn output_body_id_is_a_separate_namespace_from_the_store_id() {
    let h = harness();
    // Review M2: an output's config-level `id` is optional, label-derived, and
    // routable (OutputRef) — a DIFFERENT namespace from the resource/store id.
    // An authored id differing from the store id must be preserved, not 422'd
    // and never rewritten.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/output-0",
            OPERATOR_TOKEN,
            &json!({
                "name": "Push",
                "body": { "id": "push-main", "kind": "rtmp", "url": "rtmp://h/x", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(
        created["body"]["id"], "push-main",
        "the authored output id is preserved verbatim"
    );

    // And a body with NO id must not have the store id injected.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/output-1",
            OPERATOR_TOKEN,
            &json!({
                "name": "HLS",
                "body": { "kind": "hls", "path": "/srv/hls", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert!(
        created["body"].get("id").is_none(),
        "no store-id injection for outputs (label-derived ids stay derived)"
    );
}

#[tokio::test]
async fn stale_if_match_wins_over_an_invalid_body_on_update() {
    let h = harness();
    // RFC 9110 §13.2.2: preconditions are evaluated before request content.
    send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({ "name": "C", "body": { "kind": "bars" } }),
        ),
    )
    .await;
    send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "C2", "body": { "kind": "bars" } }),
        ),
    )
    .await;
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "X", "body": { "kind": "flux" } }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::PRECONDITION_FAILED,
        "stale If-Match is reported before body validation"
    );
}

/// Reject-fixtures: the mirrors must REJECT what the config types reject
/// (review minor 3 — the both-accept fixture alone cannot catch a mirror that
/// is looser than the real type).
#[test]
fn openapi_mirrors_reject_what_the_config_types_reject() {
    let bad_sources = [
        json!({ "id": "s", "kind": "clock", "face": null }),
        json!({ "id": "s", "kind": "rtsp", "url": "rtsp://h/x", "wallclock": { "use": "maybe" } }),
    ];
    for doc in &bad_sources {
        assert!(
            serde_json::from_value::<multiview_config::Source>(doc.clone()).is_err(),
            "config must reject {doc}"
        );
        assert!(
            serde_json::from_value::<multiview_control::openapi_schemas::SourceBodyDoc>(
                doc.clone()
            )
            .is_err(),
            "mirror must reject {doc}"
        );
    }
    let bad_output = json!({
        "kind": "srt", "url": "srt://[2001:db8::3]:7000", "codec": "h264",
        "audio": { "mode": "both" }
    });
    assert!(serde_json::from_value::<multiview_config::Output>(bad_output.clone()).is_err());
    assert!(
        serde_json::from_value::<multiview_control::openapi_schemas::OutputBodyDoc>(bad_output)
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// ADR-W018: live source apply — per-kind `X-Multiview-Apply` truth + the
// UpsertSource/RemoveSource commands the routes enqueue for the engine drain.
// ---------------------------------------------------------------------------

/// The apply header on `resp`, as a string.
fn apply_header(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.headers()
        .get(APPLY_HEADER)
        .expect("mutation declares apply semantics")
        .to_str()
        .expect("ascii header")
        .to_owned()
}

#[tokio::test]
async fn synthetic_source_create_applies_live_and_enqueues_upsert() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({ "name": "Bars", "body": { "id": "cam1", "kind": "bars" } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        apply_header(&resp),
        "live",
        "a synthetic source mutation applies LIVE (ADR-W018)"
    );

    // The engine side receives the validated source on the bounded bus.
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::UpsertSource { source, .. }
                if source.id == "cam1"
                    && matches!(source.kind, multiview_config::SourceKind::Bars)
        )),
        "POST of a synthetic source must enqueue UpsertSource, got {drained:?}"
    );
}

#[tokio::test]
async fn synthetic_source_delete_applies_live_and_enqueues_remove() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({ "name": "Bars", "body": { "id": "cam1", "kind": "bars" } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    let resp = send(
        &h.router,
        support::delete_if_match(
            "/api/v1/sources/cam1",
            support::ADMIN_TOKEN,
            Some("W/\"1\""),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(apply_header(&resp), "live", "a delete applies LIVE");
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::RemoveSource { id, .. } if id == "cam1"
        )),
        "DELETE must enqueue RemoveSource, got {drained:?}"
    );
}

#[tokio::test]
async fn kind_change_off_synthetic_stops_the_generator_but_stays_restart() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({ "name": "Bars", "body": { "id": "cam1", "kind": "bars" } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();

    // bars -> rtsp: the stored doc only applies on restart, but the running
    // generator must STOP (a stale bars picture pretending to be the new URL
    // would be dishonest), so a RemoveSource rides the bus.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam1" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(apply_header(&resp), "restart");
    let drained = h.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            multiview_control::Command::RemoveSource { id, .. } if id == "cam1"
        )),
        "a synthetic->network kind change must enqueue RemoveSource, got {drained:?}"
    );
}

#[tokio::test]
async fn live_apply_degrades_to_restart_when_the_engine_is_gone() {
    let h = harness();
    let support::Harness {
        router, commands, ..
    } = h;
    // No engine drains the bus (the receiver is gone): the mutation is stored
    // but can only apply on restart — the header must say so honestly.
    drop(commands);
    let resp = send(
        &router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({ "name": "Bars", "body": { "id": "cam1", "kind": "bars" } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        apply_header(&resp),
        "restart",
        "with no engine draining, live apply must degrade to restart"
    );
}
