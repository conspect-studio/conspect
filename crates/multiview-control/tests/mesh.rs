//! Mesh control-route tests (Conspect, ADR-0051 / brief §11): the always-on
//! discovery status, the relay opt-in toggle (a REAL persisted toggle), and the
//! untrusted discovered-peer inventory.
//!
//! * `GET /api/v1/mesh/status` → `{discovery: "always_on", relay_enabled, role,
//!   via?, peers_count}` (role: read).
//! * `PUT /api/v1/mesh/relay` `{enabled}` → the mesh status (role: write; the
//!   toggle is persisted in the control mesh state and reflected on the next GET).
//! * `GET /api/v1/mesh/peers` → the peer list (role: read).
//! * Discovery has **NO** off switch — there is no mutating route that disables
//!   it (the spec's locked row).
//!
//! The mesh plane is control-plane-only data: these routes read/toggle a store;
//! they hold no engine handle and can never back-pressure the engine (inv #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // The locked-row test deliberately `panic!`s inside an `if` over the route
    // table (a clearer per-route message than a single `assert!`) — the test
    // intent, not a weakened assertion.
    clippy::manual_assert,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_mesh::peer::PeerObservation;
use multiview_mesh::{ClaimState, MeshState, PeerKey};
use support::{body_json, get, harness_with, send, ADMIN_TOKEN, VIEWER_TOKEN};

/// A `PUT` with a Bearer token and a JSON body.
fn put_json(path: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("encode")))
        .expect("request builds")
}

#[tokio::test]
async fn status_reports_always_on_discovery_relay_off_and_direct_role() {
    let h = harness_with(|state| state.with_mesh(Arc::new(MeshState::new())));

    let resp = send(&h.router, get("/api/v1/mesh/status", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["discovery"], "always_on", "discovery is always-on");
    assert_eq!(body["relay_enabled"], false, "relay is opt-out by default");
    assert_eq!(body["role"]["kind"], "direct", "online, no opt-in → direct");
    assert_eq!(body["peers_count"], 0, "no peers discovered yet");
    assert!(
        body.get("via").is_none() || body["via"].is_null(),
        "no via when direct"
    );
}

#[tokio::test]
async fn relay_toggle_round_trips_and_persists() {
    let mesh = Arc::new(MeshState::new());
    let h = harness_with({
        let mesh = Arc::clone(&mesh);
        move |state| state.with_mesh(mesh)
    });

    // Toggle relay ON.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/mesh/relay",
            ADMIN_TOKEN,
            &serde_json::json!({"enabled": true}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["relay_enabled"], true,
        "the PUT returns the new status"
    );
    assert_eq!(body["role"]["kind"], "relay", "online + opted-in → relay");

    // The toggle PERSISTED: a fresh GET reflects it (and the shared store too).
    let resp = send(&h.router, get("/api/v1/mesh/status", VIEWER_TOKEN)).await;
    assert_eq!(body_json(resp).await["relay_enabled"], true);
    assert!(
        mesh.relay_enabled(),
        "the toggle persisted in the shared store"
    );

    // Toggle back OFF.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/mesh/relay",
            ADMIN_TOKEN,
            &serde_json::json!({"enabled": false}),
        ),
    )
    .await;
    assert_eq!(body_json(resp).await["relay_enabled"], false);
    assert!(!mesh.relay_enabled());
}

#[tokio::test]
async fn relay_toggle_records_an_actor_attributed_audit_entry() {
    // The seam (Conspect ADR-0053 §4 / brief §10/§11): every successful relay
    // toggle is an immutable, timestamped, actor-attributed account-audit entry.
    // Drive the real PUT then read the trail back over GET /api/v1/account/audit
    // filtered to the relay-toggle kind (end-to-end, no peeking at the store).
    let mesh = Arc::new(MeshState::new());
    let h = harness_with({
        let mesh = Arc::clone(&mesh);
        move |state| state.with_mesh(mesh)
    });

    // Toggle relay ON as the admin principal (a write action).
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/mesh/relay",
            ADMIN_TOKEN,
            &serde_json::json!({"enabled": true}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The trail, filtered to relay-toggle, carries exactly one entry: the ON
    // toggle, attributed to the admin key, detailing the new state.
    let page = body_json(
        send(
            &h.router,
            get("/api/v1/account/audit?filter=relay-toggle", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let entries = page["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "the ON toggle is audited exactly once");
    let entry = &entries[0];
    assert_eq!(entry["kind"], "relay-toggle", "the right audit kind");
    assert_eq!(
        entry["actor"], "admin-key",
        "the actor is the toggling principal's key id"
    );
    assert_eq!(
        entry["detail"]["enabled"], true,
        "the detail carries the new relay state"
    );

    // Toggle OFF — a second, distinct audit entry (detail reflects the new state).
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/mesh/relay",
            ADMIN_TOKEN,
            &serde_json::json!({"enabled": false}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let page = body_json(
        send(
            &h.router,
            get("/api/v1/account/audit?filter=relay-toggle", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    let entries = page["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2, "each successful toggle is one entry");
    assert_eq!(
        entries[1]["detail"]["enabled"], false,
        "the OFF toggle's detail reflects the new state"
    );
    assert_eq!(entries[1]["actor"], "admin-key");
}

#[tokio::test]
async fn a_denied_relay_toggle_records_no_audit_entry() {
    // A forbidden toggle (a read-only Viewer) must NOT write to the trail — only
    // a SUCCESSFUL toggle is an auditable account action.
    let h = harness_with(|state| state.with_mesh(Arc::new(MeshState::new())));
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/mesh/relay",
            VIEWER_TOKEN,
            &serde_json::json!({"enabled": true}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let page = body_json(
        send(
            &h.router,
            get("/api/v1/account/audit?filter=relay-toggle", ADMIN_TOKEN),
        )
        .await,
    )
    .await;
    assert!(
        page["entries"]
            .as_array()
            .expect("entries array")
            .is_empty(),
        "a denied toggle never touched the relay state, so it is not audited"
    );
}

#[tokio::test]
async fn peers_lists_the_untrusted_inventory() {
    let mesh = Arc::new(MeshState::new());
    // Seed two discovered peers directly into the shared store (as the announce
    // loop would).
    mesh.observe(PeerObservation {
        key: PeerKey::from_digest([0x11; 32]),
        claim_state: ClaimState::Claimed,
        observed_at: std::time::Duration::from_secs(5),
    });
    mesh.observe(PeerObservation {
        key: PeerKey::from_digest([0x22; 32]),
        claim_state: ClaimState::Unclaimed,
        observed_at: std::time::Duration::from_secs(6),
    });
    let h = harness_with({
        let mesh = Arc::clone(&mesh);
        move |state| state.with_mesh(mesh)
    });

    let resp = send(&h.router, get("/api/v1/mesh/peers", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let peers = body.as_array().expect("peers is an array");
    assert_eq!(peers.len(), 2);
    // Each peer is UNTRUSTED: relaying_for_us is false; the id is pure hex.
    for peer in peers {
        assert_eq!(
            peer["relaying_for_us"], false,
            "discovered peers are untrusted"
        );
        let id = peer["key"].as_str().expect("hex key");
        assert_eq!(id.len(), 64);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "the id is pure hex, never a raw identifier"
        );
    }
    // The status peer count matches.
    let status = body_json(send(&h.router, get("/api/v1/mesh/status", VIEWER_TOKEN)).await).await;
    assert_eq!(status["peers_count"], 2);
}

#[tokio::test]
async fn status_and_peers_require_authentication() {
    let h = harness_with(|state| state.with_mesh(Arc::new(MeshState::new())));
    for path in ["/api/v1/mesh/status", "/api/v1/mesh/peers"] {
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request");
        let resp = send(&h.router, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{path} requires auth"
        );
    }
}

#[tokio::test]
async fn the_relay_toggle_requires_write_role() {
    let h = harness_with(|state| state.with_mesh(Arc::new(MeshState::new())));
    // A read-only Viewer may not toggle relay.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/mesh/relay",
            VIEWER_TOKEN,
            &serde_json::json!({"enabled": true}),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "toggling relay is a write action"
    );
}

#[test]
fn there_is_no_route_to_disable_discovery() {
    // The spec's LOCKED row: discovery is always-on and has NO mutating endpoint.
    // Assert the REST surface enumerates the read/toggle mesh routes but NOTHING
    // that could turn discovery off (no PUT/POST/DELETE on /mesh/discovery, no
    // /mesh/status mutation).
    use multiview_control::openapi::ApiDoc;
    let routes = ApiDoc::rest_routes();

    // The three intended mesh routes ARE present.
    assert!(routes.contains(&("GET", "/api/v1/mesh/status")));
    assert!(routes.contains(&("PUT", "/api/v1/mesh/relay")));
    assert!(routes.contains(&("GET", "/api/v1/mesh/peers")));

    // No route mutates discovery, and /mesh/status is read-only (no PUT/POST/DELETE).
    for (method, path) in routes {
        if path.contains("/mesh/discovery") {
            panic!("no /mesh/discovery route may exist — discovery has no off switch ({method} {path})");
        }
        if *path == "/api/v1/mesh/status" {
            assert_eq!(
                *method, "GET",
                "/mesh/status is read-only; no mutation of discovery"
            );
        }
    }
}

#[test]
fn the_mesh_status_doc_mirror_matches_the_real_status_shape() {
    // The route serialises the real `multiview_mesh::MeshStatus` but advertises
    // `openapi_schemas::MeshStatusDoc` (the mesh crate carries no utoipa dep). If
    // the two serde shapes drift the generated client would be wrong; pin them.
    let mesh = MeshState::new();
    let status = mesh.status();
    let status_json = serde_json::to_value(&status).expect("status serialises");
    let doc: multiview_control::openapi_schemas::MeshStatusDoc =
        serde_json::from_value(status_json.clone()).expect("Doc accepts the real status shape");
    let doc_json = serde_json::to_value(&doc).expect("Doc serialises");
    assert_eq!(
        status_json, doc_json,
        "MeshStatusDoc must mirror MeshStatus byte-for-byte (no drift)"
    );
}
