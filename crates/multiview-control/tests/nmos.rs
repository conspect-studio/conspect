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
    body_json, get, harness, patch_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

fn core(id: &str) -> ResourceCore {
    ResourceCore::new(id, "1700000000:0", format!("label-{id}"))
}

/// Seed the NMOS registry with TWO devices — one in the `SCOPED_TOKEN`
/// allowlist (`scoped-layout`) and one outside it (`dev-other`) — each with its
/// own sender + receiver linked by `device_id`. Drives the per-object
/// (BOLA, ADR-W005/ADR-W025) enumeration-filter tests on the NMOS LIST routes.
fn seed_two_devices(h: &support::Harness) {
    h.nmos.set_node(Node {
        core: core("node-1"),
        href: "http://multiview.local/".to_owned(),
        hostname: Some("multiview.local".to_owned()),
    });
    for dev in ["scoped-layout", "dev-other"] {
        h.nmos.add_device(Device {
            core: core(dev),
            node_id: "node-1".to_owned(),
            device_type: "urn:x-nmos:device:generic".to_owned(),
            senders: vec![format!("snd-{dev}")],
            receivers: vec![format!("rcv-{dev}")],
        });
        h.nmos.add_sender(Sender {
            core: core(&format!("snd-{dev}")),
            device_id: dev.to_owned(),
            flow_id: None,
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            manifest_href: None,
        });
        h.nmos.add_receiver(Receiver {
            core: core(&format!("rcv-{dev}")),
            device_id: dev.to_owned(),
            format: MediaFormat::Video,
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            subscribed_sender: None,
        });
    }
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

/// BOLA enumeration (OWASP API1, conventions §H / ADR-W005 / ADR-W025): the
/// NMOS Node API LIST routes must filter to the principal's object allowlist,
/// by parity with the IS-05 per-id `PATCH .../staged` already authorizing the
/// receiver id. A scoped operator that lists `/x-nmos/.../devices` must see ONLY
/// its allowlisted device — never enumerate out-of-scope device ids.
///
/// `SCOPED_TOKEN` is scoped to `["scoped-layout"]`; the registry holds devices
/// `scoped-layout` (IN) and `dev-other` (OUT). The scoped device list must be
/// exactly `["scoped-layout"]`; an admin still sees both.
#[tokio::test]
async fn nmos_device_list_filters_to_the_scoped_allowlist() {
    let h = harness();
    seed_two_devices(&h);

    let resp = send(&h.router, get("/x-nmos/node/v1.3/devices", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout"],
        "a scoped principal must see ONLY its allowlisted NMOS device, never enumerate others (BOLA)"
    );

    // An unscoped admin still sees both devices.
    let resp = send(&h.router, get("/x-nmos/node/v1.3/devices", ADMIN_TOKEN)).await;
    let listed = body_json(resp).await;
    assert_eq!(
        listed.as_array().unwrap().len(),
        2,
        "an unscoped admin still sees every NMOS device"
    );
}

/// The sender/receiver LIST routes filter by each resource's `device_id` link:
/// a scoped principal sees only senders/receivers whose associated device is in
/// its allowlist — by parity with the device filter. (`/api/v1/devices` is the
/// device-object surface; an NMOS sender/receiver is "device-scoped" via its
/// `device_id`.)
#[tokio::test]
async fn nmos_sender_and_receiver_lists_filter_by_device_scope() {
    let h = harness();
    seed_two_devices(&h);

    // Senders: only the one whose device_id is in scope.
    let resp = send(&h.router, get("/x-nmos/node/v1.3/senders", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let senders = body_json(resp).await;
    let ids: Vec<&str> = senders
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["snd-scoped-layout"],
        "a scoped principal sees only senders linked to an in-scope device (BOLA)"
    );

    // Receivers: likewise.
    let resp = send(&h.router, get("/x-nmos/node/v1.3/receivers", SCOPED_TOKEN)).await;
    let receivers = body_json(resp).await;
    let ids: Vec<&str> = receivers
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["rcv-scoped-layout"],
        "a scoped principal sees only receivers linked to an in-scope device (BOLA)"
    );

    // An unscoped admin still sees both senders and both receivers.
    let resp = send(&h.router, get("/x-nmos/node/v1.3/senders", ADMIN_TOKEN)).await;
    assert_eq!(
        body_json(resp).await.as_array().unwrap().len(),
        2,
        "an unscoped admin sees every NMOS sender"
    );
    let resp = send(&h.router, get("/x-nmos/node/v1.3/receivers", ADMIN_TOKEN)).await;
    assert_eq!(
        body_json(resp).await.as_array().unwrap().len(),
        2,
        "an unscoped admin sees every NMOS receiver"
    );
}

/// BOLA per-object (OWASP API1, ADR-W005/ADR-W025): the IS-05 single-receiver
/// connection READ (`GET .../single/receivers/{id}/active`) must authorize the
/// receiver — else a scoped principal reads the live transport/connection state
/// of a receiver outside its scope by probing the id. The READ honours the
/// device-link scope (visibility): a principal scoped to a receiver's DEVICE may
/// read that receiver (the write path is stricter — see
/// `staging_a_connection_uses_strict_own_receiver_id_authz`).
///
/// `SCOPED_TOKEN` (allowlist `["scoped-layout"]`, which is `rcv-scoped-layout`'s
/// device) reading the OUT-of-scope receiver `rcv-dev-other` must be `403`;
/// reading its in-device receiver `rcv-scoped-layout` must succeed (`200`).
#[tokio::test]
async fn nmos_active_connection_read_is_object_scoped() {
    let h = harness();
    seed_two_devices(&h);

    // Out-of-scope receiver id: denied.
    let resp = send(
        &h.router,
        get(
            "/x-nmos/connection/v1.1/single/receivers/rcv-dev-other/active",
            SCOPED_TOKEN,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not read an out-of-scope receiver's connection state (BOLA)"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");

    // In-scope receiver id: still readable (the guard does not over-restrict).
    let resp = send(
        &h.router,
        get(
            "/x-nmos/connection/v1.1/single/receivers/rcv-scoped-layout/active",
            SCOPED_TOKEN,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a scoped principal may read its own in-scope receiver's connection state"
    );
}

/// Pins the deliberate READ/WRITE authz asymmetry on the IS-05 receiver
/// (ADR-W025): the staged-connection WRITE (`PATCH .../staged`) authorizes
/// **strictly the receiver's own id** — it does NOT widen to the device-link the
/// READ honours. A mutation on a receiver is not implied by being scoped to that
/// receiver's device; so a `device`-scoped principal can READ but cannot STAGE.
///
/// `SCOPED_TOKEN` is scoped to `["scoped-layout"]` — that is `rcv-scoped-layout`'s
/// DEVICE, not the receiver id. It may read the receiver's connection state (the
/// device-link READ gate, asserted above) but must be `403` staging it (the
/// strict own-receiver-id WRITE gate). This is the non-weakening guarantee: the
/// write stays exactly the pre-existing `authorize_object(receiver_id)` behaviour.
#[tokio::test]
async fn staging_a_connection_uses_strict_own_receiver_id_authz() {
    let h = harness();
    seed_two_devices(&h);

    let request = serde_json::json!({
        "master_enable": true,
        "activation": { "mode": "activate_immediate" },
        "transport_params": [{
            "destination_ip": "239.0.0.1",
            "destination_port": 5004,
            "rtp_enabled": true
        }]
    });

    // The DEVICE-scoped principal may READ the receiver (device-link visibility)…
    let resp = send(
        &h.router,
        get(
            "/x-nmos/connection/v1.1/single/receivers/rcv-scoped-layout/active",
            SCOPED_TOKEN,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the device-scoped principal may read its device's receiver"
    );

    // …but may NOT STAGE it: the write authorizes the receiver's own id
    // (`rcv-scoped-layout`), which is NOT in the allowlist `["scoped-layout"]`.
    // Staging through the device-link would be a privilege widening on a mutation
    // surface — forbidden by design (ADR-W025).
    let resp = send(
        &h.router,
        patch_json(
            "/x-nmos/connection/v1.1/single/receivers/rcv-scoped-layout/staged",
            SCOPED_TOKEN,
            &request,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "staging must use strict own-receiver-id authz — a device-scoped principal cannot stage \
         a receiver whose own id is not allowlisted (non-weakening WRITE)"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");

    // An unscoped admin may stage it (positive control: the gate does not block
    // a legitimately-authorized principal).
    let resp = send(
        &h.router,
        patch_json(
            "/x-nmos/connection/v1.1/single/receivers/rcv-scoped-layout/staged",
            ADMIN_TOKEN,
            &request,
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unscoped admin may stage the receiver"
    );
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
