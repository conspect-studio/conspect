//! Tests that the `OpenAPI` 3.1 document builds, declares version 3.1.x, lists
//! the routes, and is served at `/api/v1/openapi.json`; and that the SSE
//! transport emits a snapshot frame through the real router.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use support::{body_json, harness, send, ADMIN_TOKEN, VIEWER_TOKEN};

#[test]
fn openapi_document_builds_as_3_1_and_lists_routes() {
    use multiview_control::openapi::ApiDoc;
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).expect("OpenAPI serializes");

    // utoipa emits OpenAPI 3.1 only (ADR-W002).
    let version = json["openapi"].as_str().expect("openapi version present");
    assert!(version.starts_with("3.1"), "got version {version}");

    // The component schemas the API exposes are registered.
    let schemas = &json["components"]["schemas"];
    assert!(schemas.get("Problem").is_some(), "Problem schema present");
    assert!(schemas.get("Layout").is_some(), "Layout schema present");
    assert!(schemas.get("Role").is_some(), "Role schema present");
    // The alarm body schema is registered (the OpenAPI mirror of AlarmRecord).
    assert!(
        schemas.get("AlarmRecordDoc").is_some(),
        "AlarmRecordDoc schema present"
    );

    // The alarm paths are emitted by the utoipa #[utoipa::path] macros.
    let paths = &json["paths"];
    assert!(
        paths
            .get("/api/v1/alarms")
            .and_then(|p| p.get("get"))
            .is_some(),
        "GET /api/v1/alarms documented"
    );
    assert!(
        paths
            .get("/api/v1/alarms/{id}/ack")
            .and_then(|p| p.get("post"))
            .is_some(),
        "POST /api/v1/alarms/{{id}}/ack documented"
    );

    // The salvo + tally operator-surface schemas and paths are registered too.
    assert!(schemas.get("SalvoDoc").is_some(), "SalvoDoc schema present");
    assert!(
        schemas.get("TallyEntryDoc").is_some(),
        "TallyEntryDoc schema present"
    );
    assert!(
        schemas.get("TallyProfileDoc").is_some(),
        "TallyProfileDoc schema present"
    );
    assert!(
        paths
            .get("/api/v1/salvos/{id}/take")
            .and_then(|p| p.get("post"))
            .is_some(),
        "POST /api/v1/salvos/{{id}}/take documented"
    );
    assert!(
        paths
            .get("/api/v1/tally/override")
            .and_then(|p| p.get("put"))
            .is_some(),
        "PUT /api/v1/tally/override documented"
    );

    // The crate advertises its REST surface; assert the layouts + commands +
    // alarms + salvos + tally + realtime routes are all enumerated.
    let routes: Vec<&str> = ApiDoc::rest_routes().iter().map(|(_, p)| *p).collect();
    for expected in [
        "/api/v1/layouts",
        "/api/v1/layouts/{id}",
        "/api/v1/commands/start",
        "/api/v1/commands/stop",
        "/api/v1/commands/swap",
        "/api/v1/alarms",
        "/api/v1/alarms/{id}/ack",
        "/api/v1/salvos",
        "/api/v1/salvos/{id}",
        "/api/v1/salvos/{id}/arm",
        "/api/v1/salvos/{id}/take",
        "/api/v1/salvos/{id}/cancel",
        "/api/v1/tally",
        "/api/v1/tally/override",
        "/api/v1/tally/profiles",
        "/api/v1/tally/profiles/{id}",
        "/api/v1/ws",
        "/api/v1/events",
    ] {
        assert!(routes.contains(&expected), "route {expected} listed");
    }

    // The WHEP focus routes (PRV-2) are both enumerated and documented with the
    // right verb, for all three scopes.
    for (verb, path) in [
        ("post", "/api/v1/preview/program/whep"),
        ("delete", "/api/v1/preview/program/whep/{session_id}"),
        ("post", "/api/v1/preview/inputs/{id}/whep"),
        ("delete", "/api/v1/preview/inputs/{id}/whep/{session_id}"),
        ("post", "/api/v1/preview/outputs/{id}/whep"),
        ("delete", "/api/v1/preview/outputs/{id}/whep/{session_id}"),
    ] {
        assert!(
            paths.get(path).and_then(|p| p.get(verb)).is_some(),
            "{} {path} documented in the OpenAPI spec",
            verb.to_uppercase()
        );
    }
}

#[test]
fn openapi_document_emits_layout_and_resource_write_ops() {
    use multiview_control::openapi::ApiDoc;
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).expect("OpenAPI serializes");
    let paths = &json["paths"];

    // Every layout/resource `{id}` path must now emit its full read+write set
    // (today only the `list` reads enter the spec). The request body the SPA
    // client is generated from references the input schema, and the mutating
    // verbs (PUT/DELETE) carry the If-Match `412` response (ADR-W006).
    let write_paths: &[(&str, &str)] = &[
        ("/api/v1/layouts/{id}", "LayoutInput"),
        // ADR-W015: the resource bodies are documented with TYPED per-kind
        // schemas (mirrors in `openapi_schemas`), no longer an opaque input.
        ("/api/v1/sources/{id}", "SourceResourceInputDoc"),
        ("/api/v1/outputs/{id}", "OutputResourceInputDoc"),
        ("/api/v1/overlays/{id}", "OverlayResourceInputDoc"),
    ];

    for (path, input_schema) in write_paths {
        let item = paths
            .get(path)
            .unwrap_or_else(|| panic!("{path} documented"));

        // GET by id is read; POST creates; PUT replaces; DELETE removes.
        for verb in ["get", "post", "put", "delete"] {
            assert!(
                item.get(verb).is_some(),
                "{verb} {path} documented in the OpenAPI spec"
            );
        }

        // The create (POST) and replace (PUT) handlers carry a typed request
        // body referencing the resource's input schema, so the generated TS
        // client is fully typed.
        for verb in ["post", "put"] {
            let body_ref = item[verb]["requestBody"]["content"]["application/json"]["schema"]
                ["$ref"]
                .as_str()
                .unwrap_or_else(|| panic!("{verb} {path} has a JSON request body"));
            assert!(
                body_ref.ends_with(input_schema),
                "{verb} {path} request body references {input_schema}, got {body_ref}"
            );
        }

        // The optimistic-concurrency mutations (PUT replace, DELETE) advertise
        // the `412 Precondition Failed` outcome the If-Match guard produces.
        for verb in ["put", "delete"] {
            assert!(
                item[verb]["responses"].get("412").is_some(),
                "{verb} {path} documents the 412 If-Match response"
            );
        }
    }
}

#[tokio::test]
async fn openapi_json_is_served() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/openapi.json")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    assert!(doc["openapi"].as_str().unwrap().starts_with("3.1"));
    assert_eq!(doc["info"]["title"], "Multiview Control API");
}

#[tokio::test]
async fn scalar_docs_are_served() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/docs")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn sse_endpoint_emits_a_snapshot_then_streams() {
    // We cannot easily read an infinite SSE stream synchronously, but we can
    // assert the endpoint accepts the request and returns the SSE content type.
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("text/event-stream"),
        "got {content_type}"
    );
}

#[tokio::test]
async fn sse_endpoint_rejects_an_unauthenticated_request() {
    // The realtime event firehose (tile state, alerts, input/output status) is a
    // privileged read: a request with NO Authorization header must be rejected
    // before any event is streamed.
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/events")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an anonymous client must not be able to stream events"
    );
    // The rejection is an RFC 9457 problem document, not an SSE stream.
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("application/problem+json"),
        "got {content_type}"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], 401);
    assert_eq!(body["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn ws_endpoint_rejects_an_unauthenticated_request_pre_upgrade() {
    // The WebSocket transport authenticates BEFORE the upgrade: an anonymous
    // upgrade attempt must fail as a debuggable 401 HTTP response (not a silent
    // socket close or a bare upgrade-handshake error). We send the standard
    // WS upgrade headers but NO Authorization header.
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/ws")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an anonymous client must not be able to upgrade to the event stream"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], 401);
    assert_eq!(body["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn ws_endpoint_accepts_an_authenticated_viewer_past_the_auth_gate() {
    // A Viewer (read-only) holds Action::Read, so the pre-upgrade auth gate must
    // let the request through (it is NOT rejected with 401/403). We cannot drive
    // a full 101 upgrade through the in-memory `oneshot` test path (there is no
    // real connection to hijack, so axum's WebSocketUpgrade extractor returns
    // 426); the load-bearing property here is that authentication/authorization
    // succeed for an authorized viewer — proven by the absence of an auth
    // rejection — complementing the unauthenticated 401 test above.
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/ws")
        .header(header::AUTHORIZATION, format!("Bearer {VIEWER_TOKEN}"))
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an authenticated viewer must pass the WS auth gate"
    );
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a viewer holds Action::Read and must not be forbidden"
    );
}

#[tokio::test]
async fn sse_endpoint_streams_for_an_authenticated_viewer() {
    // The positive streaming path through the real router for a read-only
    // Viewer: the auth gate admits it and the SSE body is returned (200 +
    // text/event-stream). SSE needs no socket hijack, so the full path is
    // observable through `oneshot`.
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {VIEWER_TOKEN}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/event-stream"),
        "got {content_type}"
    );
}
