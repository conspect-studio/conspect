//! End-to-end tests for the AMWA NMOS Node API surface, driven through the real
//! axum router via `tower::oneshot` (no sockets).
//!
//! Covers IS-04 resource reads (node/devices/senders/receivers), IS-05 staged
//! connection (`PATCH .../staged` with an immediate activation), the auth gate
//! on every endpoint, and that the `OpenAPI` document advertises the NMOS routes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_control::{
    Device, MediaFormat, Node, Receiver, ResourceCore, Sender, TransportParams,
};
use support::{
    body_json, get, harness, patch_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN,
};

fn core(id: &str) -> ResourceCore {
    ResourceCore::new(id, "1700000000:0", format!("label-{id}"))
}

/// Seed a harness's NMOS registry with one node, device, sender, and receiver.
fn seed(h: &support::Harness) {
    h.nmos.set_node(Node {
        core: core("node-1"),
        href: "http://multiview.local/".to_owned(),
        hostname: Some("multiview.local".to_owned()),
    });
    h.nmos.add_device(Device {
        core: core("dev-1"),
        node_id: "node-1".to_owned(),
        device_type: "urn:x-nmos:device:generic".to_owned(),
        senders: vec!["snd-1".to_owned()],
        receivers: vec!["rcv-1".to_owned()],
    });
    h.nmos.add_sender(Sender {
        core: core("snd-1"),
        device_id: "dev-1".to_owned(),
        flow_id: None,
        transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
        manifest_href: None,
    });
    h.nmos.add_receiver(Receiver {
        core: core("rcv-1"),
        device_id: "dev-1".to_owned(),
        format: MediaFormat::Video,
        transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
        subscribed_sender: None,
    });
}

#[tokio::test]
async fn node_self_is_served_to_an_authenticated_reader() {
    let h = harness();
    seed(&h);
    let resp = send(&h.router, get("/x-nmos/node/v1.3/self", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], "node-1");
    // The IS-04 core fields are flat on the resource.
    assert_eq!(body["version"], "1700000000:0");
    assert_eq!(body["hostname"], "multiview.local");
}

#[tokio::test]
async fn devices_senders_receivers_are_listed() {
    let h = harness();
    seed(&h);

    let resp = send(&h.router, get("/x-nmos/node/v1.3/devices", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let devices = body_json(resp).await;
    assert_eq!(devices.as_array().unwrap().len(), 1);
    assert_eq!(devices[0]["node_id"], "node-1");

    let resp = send(&h.router, get("/x-nmos/node/v1.3/senders", VIEWER_TOKEN)).await;
    let senders = body_json(resp).await;
    assert_eq!(senders[0]["id"], "snd-1");

    let resp = send(&h.router, get("/x-nmos/node/v1.3/receivers", VIEWER_TOKEN)).await;
    let receivers = body_json(resp).await;
    assert_eq!(receivers[0]["format"], "video");
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected_on_every_endpoint() {
    let h = harness();
    seed(&h);
    for path in [
        "/x-nmos/node/v1.3/self",
        "/x-nmos/node/v1.3/devices",
        "/x-nmos/node/v1.3/senders",
        "/x-nmos/node/v1.3/receivers",
        "/x-nmos/connection/v1.1/single/receivers/rcv-1/active",
    ] {
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = send(&h.router, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "anonymous GET {path} must be rejected"
        );
    }
}

#[tokio::test]
async fn self_is_404_when_no_node_is_configured() {
    let h = harness();
    // No seed: the registry is empty.
    let resp = send(&h.router, get("/x-nmos/node/v1.3/self", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["status"], 404);
}

#[tokio::test]
async fn staging_an_immediate_connection_activates_it_over_http() {
    let h = harness();
    seed(&h);

    // Active connection starts empty.
    let resp = send(
        &h.router,
        get(
            "/x-nmos/connection/v1.1/single/receivers/rcv-1/active",
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let before = body_json(resp).await;
    assert_eq!(before["active"].as_array().unwrap().len(), 0);

    // PATCH the staged endpoint with an immediate activation.
    let request = serde_json::json!({
        "master_enable": true,
        "activation": { "mode": "activate_immediate" },
        "transport_params": [{
            "destination_ip": "239.0.0.1",
            "destination_port": 5004,
            "rtp_enabled": true
        }],
        "sender_id": "snd-1"
    });
    let resp = send(
        &h.router,
        patch_json(
            "/x-nmos/connection/v1.1/single/receivers/rcv-1/staged",
            OPERATOR_TOKEN,
            &request,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let state = body_json(resp).await;
    // The immediate activation moved the staged params to active.
    assert_eq!(state["active"][0]["destination_port"], 5004);
    assert_eq!(state["master_enable"], true);
    assert!(state["staged"].is_null());

    // And the activation is durable: a fresh GET sees the active params.
    let resp = send(
        &h.router,
        get(
            "/x-nmos/connection/v1.1/single/receivers/rcv-1/active",
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let after = body_json(resp).await;
    assert_eq!(after["active"][0]["destination_ip"], "239.0.0.1");

    // The registry (read directly) agrees with what HTTP reported.
    let direct = h.nmos.connection("rcv-1").unwrap();
    assert_eq!(
        direct.active,
        vec![TransportParams {
            destination_ip: Some("239.0.0.1".to_owned()),
            destination_port: Some(5004),
            source_ip: None,
            rtp_enabled: Some(true),
        }]
    );
}

#[tokio::test]
async fn staging_on_an_unknown_receiver_is_404() {
    let h = harness();
    seed(&h);
    let request = serde_json::json!({
        "activation": { "mode": "activate_immediate" },
        "transport_params": []
    });
    let resp = send(
        &h.router,
        patch_json(
            "/x-nmos/connection/v1.1/single/receivers/ghost/staged",
            OPERATOR_TOKEN,
            &request,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_viewer_may_not_stage_a_connection() {
    // Staging is a write action; a read-only viewer must be forbidden.
    let h = harness();
    seed(&h);
    let request = serde_json::json!({
        "activation": { "mode": "activate_immediate" },
        "transport_params": []
    });
    let resp = send(
        &h.router,
        patch_json(
            "/x-nmos/connection/v1.1/single/receivers/rcv-1/staged",
            VIEWER_TOKEN,
            &request,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_admin_may_read_the_node_too() {
    let h = harness();
    seed(&h);
    let resp = send(&h.router, get("/x-nmos/node/v1.3/self", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn openapi_document_advertises_the_nmos_routes() {
    use multiview_control::openapi::ApiDoc;
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).expect("OpenAPI serializes");
    let paths = &json["paths"];
    assert!(
        paths
            .get("/x-nmos/node/v1.3/self")
            .and_then(|p| p.get("get"))
            .is_some(),
        "GET /x-nmos/node/v1.3/self documented"
    );
    assert!(
        paths
            .get("/x-nmos/connection/v1.1/single/receivers/{id}/staged")
            .and_then(|p| p.get("patch"))
            .is_some(),
        "PATCH staged documented"
    );
    // The NMOS resource schemas are registered.
    let schemas = &json["components"]["schemas"];
    assert!(schemas.get("Node").is_some(), "Node schema present");
    assert!(
        schemas.get("ConnectionRequest").is_some(),
        "ConnectionRequest schema present"
    );

    // The advertised REST route list includes the NMOS surface.
    let routes: Vec<&str> = ApiDoc::rest_routes().iter().map(|(_, p)| *p).collect();
    for expected in [
        "/x-nmos/node/v1.3/self",
        "/x-nmos/node/v1.3/devices",
        "/x-nmos/node/v1.3/senders",
        "/x-nmos/node/v1.3/receivers",
        "/x-nmos/connection/v1.1/single/receivers/{id}/staged",
    ] {
        assert!(routes.contains(&expected), "route {expected} listed");
    }
}
