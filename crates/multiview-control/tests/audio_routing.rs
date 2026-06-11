//! End-to-end tests for the **audio routing** singleton document at
//! `/api/v1/audio-routing`: 404-free GET (an unconfigured document reports
//! `configured: false`), PUT-replace with typed validation against
//! `multiview_config::AudioRouting` (422 + field path), the routing block's own
//! semantic validation (duplicate tracks/inputs, reserved `prog`, an all-muted
//! program bus), `ETag`/`If-Match` optimistic concurrency (412/428), RBAC, and
//! the `X-Multiview-Apply: restart` apply-semantics header. Mirrors
//! `tests/sources.rs` structurally; cross-checks against the declared sources
//! happen at `GET /api/v1/config/export`, not here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{body_json, get, harness, send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN};

const APPLY_HEADER: &str = "x-multiview-apply";

/// A valid two-route document: one stereo camera on the program bus with a
/// discrete clean track, one muted commentary feed routed to its own track.
fn routing_doc() -> serde_json::Value {
    json!({
        "sample_rate_hz": 48_000,
        "routes": [
            {
                "input_id": "cam1",
                "channels": { "kind": "stereo" },
                "target_track": "cam1-clean",
                "language": "eng",
                "title": "Camera 1",
                "include_in_program_bus": true,
                "gain_db": -3.0,
                "mute": false
            },
            {
                "input_id": "comms",
                "channels": { "kind": "mono" },
                "target_track": "commentary",
                "include_in_program_bus": false,
                "gain_db": 0.0,
                "mute": true
            }
        ]
    })
}

/// Build a `PUT /api/v1/audio-routing` request (optional `If-Match`).
fn put_routing(
    token: &str,
    if_match: Option<&str>,
    body: &serde_json::Value,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/v1/audio-routing")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = if_match {
        builder = builder.header(header::IF_MATCH, value);
    }
    builder
        .body(axum::body::Body::from(serde_json::to_vec(body).unwrap()))
        .expect("request should build")
}

#[tokio::test]
async fn get_unconfigured_is_404_free_and_reports_configured_false() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/audio-routing", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK, "the singleton never 404s");
    assert_eq!(
        resp.headers()
            .get(header::ETAG)
            .expect("GET carries the document ETag")
            .to_str()
            .unwrap(),
        "W/\"1\"",
        "the unconfigured document is version 1"
    );
    let body = body_json(resp).await;
    assert_eq!(body["configured"], false);
    assert_eq!(body["routing"], serde_json::Value::Null);
    // The program bus is ALWAYS selectable, even with no routing configured.
    assert_eq!(body["selectable_tracks"], json!(["prog"]));
}

#[tokio::test]
async fn put_then_get_round_trips_with_etag_and_apply_header() {
    let h = harness();

    let resp = send(
        &h.router,
        put_routing(OPERATOR_TOKEN, Some("W/\"1\""), &routing_doc()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"2\"",
        "a successful replace bumps the version"
    );
    assert_eq!(
        resp.headers()
            .get(APPLY_HEADER)
            .expect("PUT declares apply semantics")
            .to_str()
            .unwrap(),
        "restart"
    );
    let body = body_json(resp).await;
    assert_eq!(body["configured"], true);
    assert_eq!(body["routing"]["sample_rate_hz"], 48_000);
    assert_eq!(body["routing"]["routes"][0]["input_id"], "cam1");
    assert_eq!(body["routing"]["routes"][0]["gain_db"], -3.0);
    assert_eq!(body["routing"]["routes"][1]["mute"], true);
    // The selectable set is the program bus plus the declared tracks, in
    // declaration order.
    assert_eq!(
        body["selectable_tracks"],
        json!(["prog", "cam1-clean", "commentary"])
    );

    let resp = send(&h.router, get("/api/v1/audio-routing", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"2\""
    );
    let body = body_json(resp).await;
    assert_eq!(body["configured"], true);
    assert_eq!(
        body["routing"]["routes"][0]["channels"],
        json!({ "kind": "stereo" }),
        "channels round-trip as the internally-tagged form"
    );
}

#[tokio::test]
async fn put_without_if_match_is_precondition_required() {
    let h = harness();
    let resp = send(&h.router, put_routing(OPERATOR_TOKEN, None, &routing_doc())).await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/precondition-required");
}

#[tokio::test]
async fn put_with_stale_if_match_is_412_and_preserves_the_stored_document() {
    let h = harness();
    let resp = send(
        &h.router,
        put_routing(OPERATOR_TOKEN, Some("W/\"1\""), &routing_doc()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let clobber = json!({ "sample_rate_hz": 44_100, "routes": [] });
    let resp = send(
        &h.router,
        put_routing(OPERATOR_TOKEN, Some("W/\"1\""), &clobber),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/version-conflict");

    let resp = send(&h.router, get("/api/v1/audio-routing", VIEWER_TOKEN)).await;
    let body = body_json(resp).await;
    assert_eq!(
        body["routing"]["sample_rate_hz"], 48_000,
        "the clobbering write was rejected"
    );
}

#[tokio::test]
async fn stale_if_match_wins_over_an_invalid_body() {
    // RFC 9110 §13.2.2: preconditions are evaluated before request content.
    let h = harness();
    send(
        &h.router,
        put_routing(OPERATOR_TOKEN, Some("W/\"1\""), &routing_doc()),
    )
    .await;
    let resp = send(
        &h.router,
        put_routing(
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "sample_rate_hz": "not-a-rate" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn put_with_a_mistyped_field_is_422_naming_the_field_path() {
    let h = harness();
    let resp = send(
        &h.router,
        put_routing(
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "sample_rate_hz": "fast", "routes": [] }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or("")
            .contains("sample_rate_hz"),
        "detail names the offending field path, got: {}",
        problem["detail"]
    );
}

#[tokio::test]
async fn put_with_an_unknown_field_is_422() {
    let h = harness();
    let resp = send(
        &h.router,
        put_routing(
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "sample_rate_hz": 48_000, "routes": [], "bitrate": 256 }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the document schema denies unknown fields"
    );
}

#[tokio::test]
async fn put_with_an_unknown_channel_layout_is_422() {
    let h = harness();
    let resp = send(
        &h.router,
        put_routing(
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    { "input_id": "cam1", "channels": { "kind": "octophonic" } }
                ]
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn semantic_violations_are_422_naming_the_violation() {
    let h = harness();
    // Each case is well-typed but violates the routing block's own validation.
    let cases: [(&str, serde_json::Value); 6] = [
        (
            "non-finite gain (JSON literal overflows f32 to inf)",
            json!({ "sample_rate_hz": 48000, "routes": [
                { "input_id": "cam1", "channels": { "kind": "stereo" }, "gain_db": 1e39 }
            ] }),
        ),
        (
            "zero sample rate",
            json!({ "sample_rate_hz": 0, "routes": [] }),
        ),
        (
            "duplicate target_track",
            json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    { "input_id": "a", "channels": { "kind": "stereo" }, "target_track": "clean" },
                    { "input_id": "b", "channels": { "kind": "stereo" }, "target_track": "clean" }
                ]
            }),
        ),
        (
            "duplicate input_id",
            json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    { "input_id": "a", "channels": { "kind": "stereo" } },
                    { "input_id": "a", "channels": { "kind": "mono" } }
                ]
            }),
        ),
        (
            "reserved program track name",
            json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    { "input_id": "a", "channels": { "kind": "stereo" }, "target_track": "prog" }
                ]
            }),
        ),
        (
            "all included inputs muted",
            json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    {
                        "input_id": "a",
                        "channels": { "kind": "stereo" },
                        "include_in_program_bus": true,
                        "mute": true
                    }
                ]
            }),
        ),
    ];
    for (label, doc) in &cases {
        let resp = send(&h.router, put_routing(OPERATOR_TOKEN, Some("W/\"1\""), doc)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} must be rejected"
        );
        let problem = body_json(resp).await;
        assert_eq!(problem["type"], "/problems/validation", "{label}");
    }
}

#[tokio::test]
async fn a_rejected_put_does_not_bump_the_version() {
    let h = harness();
    let resp = send(
        &h.router,
        put_routing(
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({ "sample_rate_hz": 0, "routes": [] }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // The document is untouched: still version 1, still unconfigured, and the
    // original ETag still satisfies a subsequent valid PUT.
    let resp = send(&h.router, get("/api/v1/audio-routing", VIEWER_TOKEN)).await;
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        "W/\"1\""
    );
    let body = body_json(resp).await;
    assert_eq!(body["configured"], false);
    let resp = send(
        &h.router,
        put_routing(OPERATOR_TOKEN, Some("W/\"1\""), &routing_doc()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_source_reference_is_accepted_at_put_time() {
    // The document boundary validates the routing block's INTERNAL consistency;
    // resolution against the declared sources happens when the whole config is
    // composed (`GET /api/v1/config/export`). A route naming a source that does
    // not exist yet must therefore be storable.
    let h = harness();
    let resp = send(
        &h.router,
        put_routing(
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    {
                        "input_id": "not-created-yet",
                        "channels": { "kind": "stereo" },
                        "include_in_program_bus": true
                    }
                ]
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn get_requires_authentication() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/audio-routing")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn viewer_may_read_but_not_write() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/audio-routing", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(
        &h.router,
        put_routing(VIEWER_TOKEN, Some("W/\"1\""), &routing_doc()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Accept-fixtures: the `OpenAPI` mirror schemas must accept every document the
/// real `multiview_config` types accept (ADR-W015 drift pin).
#[test]
fn openapi_mirrors_accept_what_the_config_types_accept() {
    let docs = [
        json!({ "sample_rate_hz": 48_000 }),
        json!({ "sample_rate_hz": 48_000, "routes": [] }),
        routing_doc(),
        json!({
            "sample_rate_hz": 96_000,
            "routes": [
                { "input_id": "fx", "channels": { "kind": "five_point_one" },
                  "target_track": "fx-bed", "include_in_program_bus": true,
                  "gain_db": 1.5, "mute": false }
            ]
        }),
    ];
    for doc in &docs {
        let real: Result<multiview_config::AudioRouting, _> = serde_json::from_value(doc.clone());
        let mirror: Result<multiview_control::openapi_schemas::AudioRoutingDoc, _> =
            serde_json::from_value(doc.clone());
        assert!(real.is_ok(), "config rejects {doc}: {:?}", real.err());
        assert!(mirror.is_ok(), "mirror rejects {doc}: {:?}", mirror.err());
    }
}

/// Reject-fixtures: the mirrors must REJECT what the config types reject (a
/// both-accept fixture alone cannot catch a mirror looser than the real type).
#[test]
fn openapi_mirrors_reject_what_the_config_types_reject() {
    let docs = [
        // Missing the required sample rate.
        json!({ "routes": [] }),
        // Unknown top-level field (deny_unknown_fields).
        json!({ "sample_rate_hz": 48_000, "routes": [], "bitrate": 256 }),
        // Unknown channel layout tag.
        json!({
            "sample_rate_hz": 48_000,
            "routes": [ { "input_id": "a", "channels": { "kind": "octophonic" } } ]
        }),
        // Unknown per-route field (deny_unknown_fields on the route).
        json!({
            "sample_rate_hz": 48_000,
            "routes": [
                { "input_id": "a", "channels": { "kind": "stereo" }, "pan": -1.0 }
            ]
        }),
    ];
    for doc in &docs {
        assert!(
            serde_json::from_value::<multiview_config::AudioRouting>(doc.clone()).is_err(),
            "config must reject {doc}"
        );
        assert!(
            serde_json::from_value::<multiview_control::openapi_schemas::AudioRoutingDoc>(
                doc.clone()
            )
            .is_err(),
            "mirror must reject {doc}"
        );
    }
}

/// The generated document lists both verbs of the singleton path and registers
/// the mirror schemas.
#[test]
fn openapi_document_lists_the_audio_routing_surface() {
    use multiview_control::openapi::ApiDoc;
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let api = serde_json::to_value(&doc).expect("OpenAPI serializes");
    for verb in ["get", "put"] {
        assert!(
            api["paths"]["/api/v1/audio-routing"].get(verb).is_some(),
            "{} /api/v1/audio-routing documented",
            verb.to_uppercase()
        );
    }
    for schema in [
        "AudioChannelsDoc",
        "AudioRouteDoc",
        "AudioRoutingDoc",
        "AudioRoutingStateDoc",
    ] {
        assert!(
            api["components"]["schemas"].get(schema).is_some(),
            "{schema} schema registered"
        );
    }
    let routes: Vec<(&str, &str)> = ApiDoc::rest_routes().to_vec();
    assert!(routes.contains(&("GET", "/api/v1/audio-routing")));
    assert!(routes.contains(&("PUT", "/api/v1/audio-routing")));
}

#[tokio::test]
async fn a_successful_put_is_audited() {
    let h = harness();
    send(
        &h.router,
        put_routing(ADMIN_TOKEN, Some("W/\"1\""), &routing_doc()),
    )
    .await;
    let resp = send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let entries = body_json(resp).await;
    let arr = entries.as_array().expect("audit list is an array");
    assert!(
        arr.iter().any(|e| e["object_kind"] == "audio-routing"),
        "the audio-routing update is recorded in the audit log, got: {entries}"
    );
}
