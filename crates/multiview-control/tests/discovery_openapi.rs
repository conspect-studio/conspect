//! `OpenAPI` registration assertions for the device-discovery surface (DEV-A5,
//! ADR-M008 §6): the `/discovery/devices` scan + inventory routes and their
//! response schemas must be in the generated 3.1 document so the SPA's generated
//! client sees them. Mirrors `tests/devices_openapi.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::openapi::ApiDoc;
use utoipa::OpenApi;

#[test]
fn discovery_routes_are_in_the_document() {
    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).unwrap();
    let paths = &json["paths"];

    for (path, verb) in [
        ("/api/v1/discovery/devices", "get"),
        ("/api/v1/discovery/devices/scan", "post"),
    ] {
        assert!(
            paths.get(path).and_then(|p| p.get(verb)).is_some(),
            "OpenAPI is missing {} {path}",
            verb.to_uppercase()
        );
    }
}

#[test]
fn discovery_routes_are_advertised_in_rest_routes() {
    let routes: Vec<(&str, &str)> = ApiDoc::rest_routes().to_vec();
    for expected in [
        ("GET", "/api/v1/discovery/devices"),
        ("POST", "/api/v1/discovery/devices/scan"),
    ] {
        assert!(
            routes.contains(&expected),
            "rest_routes() is missing {expected:?}"
        );
    }
}

#[test]
fn discovery_schemas_are_registered() {
    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).unwrap();
    let schemas = &json["components"]["schemas"];
    // The discovery types live in this crate and derive `ToSchema` directly, so
    // they register under their real names (the `...Doc` shadow convention is for
    // types in other crates that cannot derive `ToSchema` locally).
    for schema in [
        "DiscoveredService",
        "DiscoveredEndpoint",
        "DiscoveryDriverKind",
        "ScanAccepted",
    ] {
        assert!(
            schemas.get(schema).is_some(),
            "OpenAPI is missing the {schema} schema"
        );
    }
}
