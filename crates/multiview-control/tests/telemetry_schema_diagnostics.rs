//! CONSPECT telemetry-schema + diagnostics-snapshot HTTP surfaces (spec §4.2/§11,
//! ADR-0052 §3/§4, ADR-0053).
//!
//! Pins:
//! * `GET /api/v1/telemetry/schema` publishes the daily-pipe schema with a
//!   version, a `sent` list, and a **`never_sent`** list whose contents are
//!   load-bearing (no media / stream URLs / hostnames / layouts / typed content);
//! * `POST /api/v1/diagnostics/snapshot` returns `202 {snapshot_id}` and the
//!   bundle is then readable at `GET /api/v1/diagnostics/{id}` (the §4.2
//!   one-button support bundle);
//! * the snapshot carries diagnostics (utilisation / reconnects / sheds /
//!   incidents), **never** media;
//! * the telemetry/diagnostics routes are advertised in the `OpenAPI` surface;
//! * **consent gates no local route** — every local surface answers identically
//!   whether telemetry consent is on or off (ADR-0052: staying off costs none of
//!   the local UI/API).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_control::openapi::ApiDoc;
use support::{
    body_json, get, harness_with, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    SCOPED_TOKEN,
};

/// The published telemetry schema lists what is sent and — load-bearing — what is
/// NEVER sent. The never-sent list is pinned so a "tidy" cannot silently leak a
/// privacy-sensitive category into the daily pipe (ADR-0052 §4, brief §8).
#[tokio::test]
async fn schema_publishes_a_version_and_a_pinned_never_sent_list() {
    let h = harness_with(|s| s);
    let resp = send(&h.router, get("/api/v1/telemetry/schema", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let version = body["version"].as_str().expect("schema carries a version");
    assert!(!version.is_empty(), "the schema version is non-empty");

    let sent = body["sent"].as_array().expect("a `sent` list");
    let sent_labels: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    // The daily pipe sends only aggregated/anonymised categories.
    for required in [
        "schema_version",
        "os_arch",
        "anonymous_digest",
        "protocol_codec_mix",
        "tile_counts",
        "performance_percentiles",
        "error_classes",
    ] {
        assert!(
            sent_labels.contains(&required),
            "the sent list must include {required}; got {sent_labels:?}"
        );
    }

    let never = body["never_sent"].as_array().expect("a `never_sent` list");
    let never_labels: Vec<&str> = never.iter().filter_map(|v| v.as_str()).collect();
    // The exhaustive privacy guarantee — none of these ever leave the machine.
    for forbidden in [
        "media",
        "stream_urls",
        "hostnames",
        "layouts",
        "typed_content",
        "raw_identifiers",
    ] {
        assert!(
            never_labels.contains(&forbidden),
            "the never-sent list must include {forbidden}; got {never_labels:?}"
        );
    }

    // No category appears in BOTH lists (a sent item can never also be never-sent).
    for label in &never_labels {
        assert!(
            !sent_labels.contains(label),
            "category {label} must not be in both sent and never_sent"
        );
    }
}

/// `POST /api/v1/diagnostics/snapshot` → `202 {snapshot_id}`, then the bundle is
/// readable at `GET /api/v1/diagnostics/{id}` (the §4.2 one-button bundle). The
/// snapshot carries diagnostics, never media.
#[tokio::test]
async fn snapshot_rides_202_then_ready_and_carries_no_media() {
    let h = harness_with(|s| s);

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/diagnostics/snapshot",
            ADMIN_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "a snapshot request is accepted (202)"
    );
    let accepted = body_json(resp).await;
    let snapshot_id = accepted["snapshot_id"]
        .as_str()
        .expect("202 returns a snapshot_id")
        .to_owned();
    assert!(!snapshot_id.is_empty());

    // The bundle is readable by id.
    let path = format!("/api/v1/diagnostics/{snapshot_id}");
    let resp = send(&h.router, get(&path, ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bundle = body_json(resp).await;
    assert_eq!(bundle["snapshot_id"], snapshot_id);
    assert_eq!(bundle["status"], "ready", "the bundle is ready to read");
    // The diagnostics section is the shared #111 context-pack `Bundle` (composed
    // from the consent-independent retention store + redacted config). Assert its
    // load-bearing shape: a metrics summary (utilisation percentiles + shed/
    // reconnect counts), an incident-marker window, the redacted config, and the
    // redaction record — proving it is logs + engine state, not media.
    assert!(
        bundle.get("diagnostics").is_some(),
        "the snapshot carries a diagnostics bundle"
    );
    let pack = &bundle["diagnostics"];
    let metrics = &pack["diagnostics"];
    assert!(
        metrics.get("cpu_p95").is_some(),
        "the metrics summary carries CPU percentiles (logs + engine state)"
    );
    assert!(
        metrics.get("reconnects").is_some() && metrics.get("shed_events").is_some(),
        "the metrics summary carries reconnect + shed counts"
    );
    assert!(
        pack.get("incidents").is_some(),
        "the incident-marker window is present"
    );
    assert!(
        pack.get("config").is_some(),
        "the redacted config section is present"
    );
    assert!(
        pack.get("redactions").is_some(),
        "the redaction record is present (never the redacted values)"
    );

    // Privacy: the bundle carries NO media (the §4.2 / §8 guarantee). A raw scan
    // of the serialized bundle must not contain a media-ish key.
    let serialized = serde_json::to_string(&bundle).expect("serialize bundle");
    for forbidden_key in ["\"media\"", "\"frame\"", "\"pixels\"", "\"stream_url\""] {
        assert!(
            !serialized.contains(forbidden_key),
            "the diagnostics bundle must never carry {forbidden_key}"
        );
    }
}

/// BOLA wholesale-enumeration leak (OWASP API1, ADR-W005/ADR-W025): the
/// diagnostics snapshot ALWAYS composes the `Config` section — the whole config,
/// every device id / `device_ref` / sync-group member, redacted only for
/// secrets/URLs (not ids). An OBJECT-scoped principal must not compose or read
/// it, exactly like the config export + support bundle (it would disclose every
/// object outside its allowlist wholesale).
///
/// `SCOPED_TOKEN` is an Operator scoped to `["scoped-layout"]` — only the new
/// whole-system scope guard can deny it (it has the Write/Read role).
#[tokio::test]
async fn scoped_principal_cannot_request_or_read_a_diagnostics_snapshot() {
    let h = harness_with(|s| s);

    // Compose (POST) is forbidden for a scoped principal.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/diagnostics/snapshot",
            SCOPED_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not request a whole-system diagnostics snapshot (BOLA)"
    );

    // Reading a snapshot composed by an unscoped admin is also forbidden.
    let compose = send(
        &h.router,
        post_json(
            "/api/v1/diagnostics/snapshot",
            ADMIN_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(compose.status(), StatusCode::ACCEPTED);
    let snapshot_id = body_json(compose).await["snapshot_id"]
        .as_str()
        .expect("202 returns a snapshot_id")
        .to_owned();
    let resp = send(
        &h.router,
        get(&format!("/api/v1/diagnostics/{snapshot_id}"), SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not read a whole-system diagnostics snapshot"
    );

    // An unscoped operator may still request one (no over-restriction).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/diagnostics/snapshot",
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "an unscoped operator may still request a diagnostics snapshot"
    );
}

/// A `GET /api/v1/diagnostics/{id}` for an unknown id is a 404 RFC-9457 problem,
/// not a panic or a fabricated bundle.
#[tokio::test]
async fn unknown_snapshot_is_404() {
    let h = harness_with(|s| s);
    let resp = send(
        &h.router,
        get("/api/v1/diagnostics/does-not-exist", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Consent gates NO local route: staying off (the default) costs none of the
/// local UI/API. We exercise representative local surfaces with consent OFF and
/// then ON and assert the status is identical — only the (future, O1-gated)
/// outbound daily pipe is governed by consent, never a local endpoint
/// (ADR-0052). This pins the "consent gates nothing locally" guardrail.
#[tokio::test]
async fn consent_gates_no_local_route() {
    // Representative local routes spanning resource reads, health, licence, mesh,
    // the telemetry schema itself, and a diagnostics snapshot.
    let local_routes = [
        "/api/v1/layouts",
        "/api/v1/sources",
        "/api/v1/health",
        "/api/v1/licence",
        "/api/v1/mesh/status",
        "/api/v1/telemetry/schema",
        "/api/v1/audit",
    ];

    // With consent OFF (default), capture each route's status.
    let off = harness_with(|s| s);
    let mut off_status = Vec::new();
    for route in local_routes {
        let resp = send(&off.router, get(route, ADMIN_TOKEN)).await;
        off_status.push(resp.status());
    }

    // Now turn consent ON via the real PUT, then re-exercise the same routes.
    let on = harness_with(|s| s);
    let put = send(
        &on.router,
        put_json(
            "/api/v1/telemetry/consent",
            ADMIN_TOKEN,
            None,
            &serde_json::json!({ "enabled": true }),
        ),
    )
    .await;
    assert_eq!(put.status(), StatusCode::OK);
    let mut on_status = Vec::new();
    for route in local_routes {
        let resp = send(&on.router, get(route, ADMIN_TOKEN)).await;
        on_status.push(resp.status());
    }

    assert_eq!(
        off_status, on_status,
        "no local route's behaviour may depend on telemetry consent"
    );
    // And every local route is reachable (none is gated to an error by consent).
    for (route, status) in local_routes.iter().zip(off_status.iter()) {
        assert!(
            status.is_success(),
            "local route {route} must be reachable regardless of consent (got {status})"
        );
    }
}

/// The telemetry + diagnostics routes are advertised in the `OpenAPI` REST surface
/// (no-drift: the served spec lists them), under the telemetry/diagnostics
/// namespaces — never under /licensing (the two-pipe separation).
#[test]
fn telemetry_and_diagnostics_routes_are_advertised() {
    let routes: Vec<(&str, &str)> = ApiDoc::rest_routes().to_vec();
    for expected in [
        ("GET", "/api/v1/telemetry/consent"),
        ("PUT", "/api/v1/telemetry/consent"),
        ("GET", "/api/v1/telemetry/schema"),
        ("POST", "/api/v1/diagnostics/snapshot"),
        ("GET", "/api/v1/diagnostics/{id}"),
    ] {
        assert!(
            routes.contains(&expected),
            "rest_routes() must advertise {expected:?}; got {routes:?}"
        );
    }

    // Two-pipe separation: the telemetry consent route is NOT under /licensing,
    // and the heartbeat-status route is NOT under /telemetry.
    assert!(
        !routes
            .iter()
            .any(|(_, p)| p.starts_with("/api/v1/licensing/") && p.contains("consent")),
        "telemetry consent must never live under /licensing (two-pipe separation)"
    );
    assert!(
        !routes
            .iter()
            .any(|(_, p)| p.starts_with("/api/v1/telemetry/") && p.contains("heartbeat")),
        "the licensing heartbeat must never live under /telemetry (two-pipe separation)"
    );
}

/// The `OpenAPI` document documents the telemetry + diagnostics paths with their
/// verbs (the SPA client is generated from this).
#[test]
fn openapi_documents_telemetry_and_diagnostics_paths() {
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).expect("OpenAPI serializes");
    let paths = &json["paths"];

    for (verb, path) in [
        ("get", "/api/v1/telemetry/consent"),
        ("put", "/api/v1/telemetry/consent"),
        ("get", "/api/v1/telemetry/schema"),
        ("post", "/api/v1/diagnostics/snapshot"),
        ("get", "/api/v1/diagnostics/{id}"),
    ] {
        assert!(
            paths.get(path).and_then(|p| p.get(verb)).is_some(),
            "{} {path} documented in the OpenAPI spec",
            verb.to_uppercase()
        );
    }
}
