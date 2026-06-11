//! `OpenAPI` registration assertions for the Devices + sync-groups surface
//! (ADR-M008/M009/W017): every route and request/response schema must be in the
//! generated 3.1 document so the SPA's generated client (DEV-A6) sees them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::openapi::ApiDoc;
use utoipa::OpenApi;

#[test]
fn devices_and_sync_group_routes_are_in_the_document() {
    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).unwrap();
    let paths = &json["paths"];

    // CRUD + status + projections.
    for (path, verb) in [
        ("/api/v1/devices", "get"),
        ("/api/v1/devices/{id}", "get"),
        ("/api/v1/devices/{id}", "post"),
        ("/api/v1/devices/{id}", "put"),
        ("/api/v1/devices/{id}", "delete"),
        ("/api/v1/devices/{id}/status", "get"),
        ("/api/v1/devices/{id}/probe", "post"),
        ("/api/v1/devices/{id}/set-mode", "post"),
        ("/api/v1/devices/{id}/reboot", "post"),
        ("/api/v1/devices/{id}/identify", "post"),
        ("/api/v1/devices/{id}/test-pattern", "post"),
        ("/api/v1/devices/{id}/source-candidates", "get"),
        ("/api/v1/devices/{id}/output-targets", "get"),
        ("/api/v1/sync-groups", "get"),
        ("/api/v1/sync-groups/{id}", "get"),
        ("/api/v1/sync-groups/{id}", "post"),
        ("/api/v1/sync-groups/{id}", "put"),
        ("/api/v1/sync-groups/{id}", "delete"),
        ("/api/v1/sync-groups/{id}/measure", "post"),
    ] {
        assert!(
            paths.get(path).and_then(|p| p.get(verb)).is_some(),
            "OpenAPI is missing {} {path}",
            verb.to_uppercase()
        );
    }
}

#[test]
fn devices_routes_are_advertised_in_rest_routes() {
    let routes: Vec<(&str, &str)> = ApiDoc::rest_routes().to_vec();
    for expected in [
        ("GET", "/api/v1/devices"),
        ("POST", "/api/v1/devices/{id}"),
        ("DELETE", "/api/v1/devices/{id}"),
        ("GET", "/api/v1/devices/{id}/status"),
        ("POST", "/api/v1/devices/{id}/set-mode"),
        ("GET", "/api/v1/devices/{id}/source-candidates"),
        ("GET", "/api/v1/sync-groups"),
        ("POST", "/api/v1/sync-groups/{id}/measure"),
    ] {
        assert!(
            routes.contains(&expected),
            "rest_routes() is missing {expected:?}"
        );
    }
}

#[test]
fn device_and_sync_group_schemas_are_registered() {
    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).unwrap();
    let schemas = &json["components"]["schemas"];
    for schema in [
        "DeviceBodyDoc",
        "DeviceResourceInputDoc",
        "DeviceDriverDoc",
        "SyncGroupBodyDoc",
        "SyncGroupResourceInputDoc",
        "DeviceStatusDoc",
        "SetModeRequest",
        "SetModeAccepted",
    ] {
        assert!(
            schemas.get(schema).is_some(),
            "OpenAPI is missing the {schema} schema"
        );
    }
}
