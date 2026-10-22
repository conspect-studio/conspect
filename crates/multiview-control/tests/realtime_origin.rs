//! SEC-13 (CSWSH): a WebSocket / `EventSource` handshake is exempt from the
//! Same-Origin Policy and CORS, so without an `Origin` check any site the victim
//! visits can `new WebSocket()` to `/api/v1/ws` and read the engine firehose. An
//! `auth_disabled` trusted-network mode makes this a zero-credential read.
//!
//! ADR-RT011 enforces an `Origin` allow-list on BOTH the WS and SSE upgrade,
//! BEFORE auth and REGARDLESS of `auth_disabled` (CSWSH needs no credential): an
//! absent `Origin` passes (non-browser clients), a present `Origin` passes iff it
//! is configured-allowlisted OR same-origin (its authority == the request `Host`),
//! and `Origin: null` is denied (fail-closed).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use multiview_control::AllowedOrigins;
use support::{harness_with, send, ADMIN_TOKEN};

// ---- Unit: the pure Origin policy ------------------------------------------

/// Default policy (empty allow-list): a present `Origin` is permitted iff its
/// authority equals the request `Host` — the same-origin embed-web SPA case.
#[test]
fn same_origin_permitted_by_default() {
    let policy = AllowedOrigins::new(Vec::new());
    assert!(
        policy.permits("http://mv.local:8080", Some("mv.local:8080")),
        "an origin whose authority equals Host is same-origin"
    );
    // Scheme-insensitive on the same-origin path (a TLS-terminating proxy makes
    // the backend see http while the browser Origin is https; authority is what
    // defeats CSWSH).
    assert!(
        policy.permits("https://mv.local", Some("mv.local")),
        "same authority across schemes is same-origin"
    );
    // Case-insensitive host.
    assert!(
        policy.permits("http://MV.Local", Some("mv.local")),
        "host comparison is case-insensitive"
    );
}

/// A foreign origin (the CSWSH attacker) is refused under the default policy.
#[test]
fn cross_origin_denied_by_default() {
    let policy = AllowedOrigins::new(Vec::new());
    assert!(
        !policy.permits("https://evil.example", Some("mv.local:8080")),
        "a foreign origin is not same-origin and is not allow-listed"
    );
    // A different port on the same host is a different origin.
    assert!(
        !policy.permits("http://mv.local:9999", Some("mv.local:8080")),
        "a different port is a different origin"
    );
}

/// `Origin: null` (sandboxed iframe / privacy mode / file://) is fail-closed.
#[test]
fn null_origin_denied() {
    let policy = AllowedOrigins::new(Vec::new());
    assert!(
        !policy.permits("null", Some("mv.local")),
        "the opaque `null` origin must be denied (fail-closed)"
    );
}

/// An explicitly-configured origin is permitted even when it is cross to `Host`
/// (the separate-web-origin / Host-rewriting-proxy case).
#[test]
fn configured_origin_permitted_across_host() {
    let policy = AllowedOrigins::new(vec!["https://ops.example".to_owned()]);
    assert!(
        policy.permits("https://ops.example", Some("mv.local:8080")),
        "a configured origin is allowed regardless of Host"
    );
    // Still fail-closed for anything not listed and not same-origin.
    assert!(
        !policy.permits("https://other.example", Some("mv.local:8080")),
        "an unlisted, non-same-origin request is still denied"
    );
}

/// An absent `Host` cannot establish same-origin, so a present non-allow-listed
/// `Origin` is denied (fail-closed).
#[test]
fn present_origin_without_host_denied() {
    let policy = AllowedOrigins::new(Vec::new());
    assert!(
        !policy.permits("http://mv.local", None),
        "no Host means same-origin cannot be proven — deny"
    );
}

// ---- Integration: the gate on the WS + SSE upgrades ------------------------

/// A cross-origin WS or SSE upgrade is refused with a 403, and — the SEC-13
/// point — this holds EVEN with auth disabled (CSWSH needs no credential).
#[tokio::test]
async fn cross_origin_rejected_on_ws_and_sse_even_when_auth_disabled() {
    let harness = harness_with(|state| state.with_auth_disabled(true));

    for path in ["/api/v1/ws", "/api/v1/events"] {
        let resp = send(
            &harness.router,
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::HOST, "mv.local")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "a cross-origin realtime upgrade must be refused on {path}, even auth-disabled"
        );
    }
}

/// The Origin gate runs BEFORE auth: a cross-origin request with an otherwise
/// VALID bearer is still refused (a stolen key on a foreign page cannot open the
/// firehose).
#[tokio::test]
async fn cross_origin_rejected_before_auth_with_valid_bearer() {
    let harness = support::harness();

    let resp = send(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/ws")
            .header(header::HOST, "mv.local")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a cross-origin upgrade is refused even with a valid credential"
    );
}

/// A request carrying MORE THAN ONE `Origin` header is ambiguous — a browser sends
/// exactly one — so it is refused, even when the first value would pass alone (a
/// same-origin value paired with a foreign one). Fail-closed on Origin ambiguity.
#[tokio::test]
async fn duplicate_origin_headers_rejected() {
    let harness = harness_with(|state| state.with_auth_disabled(true));

    for path in ["/api/v1/ws", "/api/v1/events"] {
        let resp = send(
            &harness.router,
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::HOST, "mv.local")
                // First value is same-origin (would pass ALONE); second is foreign.
                // Two Origin headers → ambiguous → denied regardless of the first.
                .header(header::ORIGIN, "http://mv.local")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "multiple Origin headers must be refused as ambiguous on {path}"
        );
    }
}

/// A same-origin SSE upgrade passes the Origin gate (auth disabled → 200): the
/// embed-web SPA served from the appliance works with zero config.
#[tokio::test]
async fn same_origin_accepted_on_sse() {
    let harness = harness_with(|state| state.with_auth_disabled(true));

    let resp = send(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/events")
            .header(header::HOST, "mv.local")
            .header(header::ORIGIN, "http://mv.local")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a same-origin SSE upgrade is accepted"
    );
}

/// A non-browser client (no `Origin` header) is not a CSWSH vector and is
/// admitted (auth disabled → 200) — the gate does not break native clients.
#[tokio::test]
async fn absent_origin_accepted_for_native_clients() {
    let harness = harness_with(|state| state.with_auth_disabled(true));

    let resp = send(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/events")
            .header(header::HOST, "mv.local")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a request with no Origin (native client) is admitted"
    );
}

/// An operator-configured allow-list origin is accepted on the SSE upgrade even
/// though it differs from `Host` (the separate-web-origin deployment).
#[tokio::test]
async fn configured_allowlist_origin_accepted_on_sse() {
    let harness = harness_with(|state| {
        state
            .with_auth_disabled(true)
            .with_allowed_origins(vec!["https://ops.example".to_owned()])
    });

    let resp = send(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/events")
            .header(header::HOST, "mv.local:8080")
            .header(header::ORIGIN, "https://ops.example")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a configured allow-list origin is accepted cross-Host"
    );
}

// ---- RED (SEC-13 hardening): the Origin is a real parse, not a raw string ----

/// A configured `null` entry must be UNREPRESENTABLE: `AllowedOrigins` parses +
/// canonicalizes at construction and drops what does not parse, so `Origin: null`
/// can never be allowed by ANY construction path — not only the config-validated
/// one (defect #4).
#[test]
fn configured_null_is_never_allowed() {
    let policy = AllowedOrigins::new(vec!["null".to_owned()]);
    assert!(
        !policy.permits("null", Some("mv.local")),
        "a `null` allow-list entry must not make `Origin: null` allowable"
    );
}

/// A configured non-http(s) origin is dropped at construction (defect #4): a bad
/// scheme can never be stored, so it can never be matched.
#[test]
fn configured_bad_scheme_is_never_allowed() {
    let policy = AllowedOrigins::new(vec!["garbage://mv.local".to_owned()]);
    assert!(
        !policy.permits("garbage://mv.local", Some("elsewhere.example")),
        "a non-http(s) configured entry must not be matchable"
    );
}

/// A configured entry carrying userinfo is dropped at construction (defect #4).
#[test]
fn configured_userinfo_is_never_allowed() {
    let policy = AllowedOrigins::new(vec!["https://user@mv.local".to_owned()]);
    assert!(
        !policy.permits("https://user@mv.local", Some("mv.local")),
        "a userinfo-bearing configured entry must not be matchable"
    );
}

/// A present request `Origin` carrying a PATH must not be leniently reduced to its
/// bare host and pass same-origin (defect #2): `https://mv.local/evil` is NOT
/// same-origin with `Host: mv.local`.
#[test]
fn path_bearing_origin_denied_same_origin() {
    let policy = AllowedOrigins::new(Vec::new());
    assert!(
        !policy.permits("https://mv.local/evil", Some("mv.local")),
        "a path-bearing Origin must not reduce to its host and pass same-origin"
    );
}

/// A present `Origin` with a PATH is refused on WS + SSE — the gate must not
/// reduce it to its bare authority and admit it as same-origin (defect #2), even
/// with auth disabled.
#[tokio::test]
async fn path_bearing_origin_rejected_on_ws_and_sse() {
    let harness = harness_with(|state| state.with_auth_disabled(true));

    for path in ["/api/v1/ws", "/api/v1/events"] {
        let resp = send(
            &harness.router,
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::HOST, "mv.local")
                .header(header::ORIGIN, "https://mv.local/evil")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "a path-bearing Origin must be refused on {path} (not reduced to same-origin)"
        );
    }
}

/// A present but non-UTF-8 `Origin` header must FAIL CLOSED (deny), not be folded
/// into the absent-Origin case and admitted (defect #3). The bytes are valid
/// header-value obs-text (0xFF is allowed on the wire) but not valid UTF-8, so
/// `HeaderValue::to_str()` fails — a present-but-unreadable Origin is not a
/// legitimate browser origin.
#[tokio::test]
async fn non_utf8_origin_rejected_on_ws_and_sse() {
    let harness = harness_with(|state| state.with_auth_disabled(true));
    let bad = HeaderValue::from_bytes(b"https://\xff.evil").unwrap();

    for path in ["/api/v1/ws", "/api/v1/events"] {
        let resp = send(
            &harness.router,
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::HOST, "mv.local")
                .header(header::ORIGIN, bad.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "a present but unreadable Origin must fail closed on {path}"
        );
    }
}
