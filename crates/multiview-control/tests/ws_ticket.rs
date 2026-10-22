//! SEC-01 (CRITICAL, CWE-598): the realtime browser auth must NOT accept the
//! durable `Bearer` API key as a `?access_token=` URL query parameter (it leaks
//! into reverse-proxy/access logs and browser history in cleartext). Instead an
//! authenticated `POST /api/v1/ws/ticket` mints a short-TTL, single-use,
//! principal-bound ticket, and the WS/SSE upgrades accept `?ticket=` and consume
//! it atomically (ADR-RT011, implementing ADR-RT005).
//!
//! These tests drive both the transport-agnostic `WsTicketStore` core (single-use,
//! TTL, scope-carry, bounded) and the real HTTP surface (mint auth, the removal of
//! the durable-bearer query, single-use over SSE).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::{Principal, Role, WsTicketStore, WS_TICKET_CAPACITY, WS_TICKET_TTL};
use support::{body_json, post_if_match, send, ADMIN_TOKEN};

/// A principal carrying a distinctive object scope, so a round-trip through the
/// ticket store can prove the FULL authorization is preserved (not just the role).
fn scoped_principal() -> Principal {
    Principal {
        key_id: "k-mint".to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(vec!["cam-3".to_owned(), "cam-4".to_owned()]),
        scoped_output_ids: Some(vec!["wall-1".to_owned()]),
        scoped_discovery_domains: Some(vec!["site-a".to_owned()]),
    }
}

/// (b) A freshly-minted ticket resolves to EXACTLY the minting principal (all
/// three scope axes) plus its RT010 live-reauth baseline generation — so the
/// ticket carries the same authorization the durable bearer would.
#[test]
fn mint_then_consume_returns_the_full_principal_and_baseline() {
    let store = WsTicketStore::new();
    let ticket = store.mint(scoped_principal(), Some(7));
    assert!(!ticket.is_empty(), "a minted ticket is a non-empty token");

    let (principal, baseline) = store
        .consume(&ticket)
        .expect("a fresh ticket consumes to its principal");
    assert_eq!(
        principal,
        scoped_principal(),
        "the full principal is carried"
    );
    assert_eq!(
        baseline,
        Some(7),
        "the RT010 baseline generation is carried"
    );
}

/// (c) SINGLE-USE: a ticket consumes exactly once; a replay is rejected.
#[test]
fn consume_is_single_use() {
    let store = WsTicketStore::new();
    let ticket = store.mint(scoped_principal(), None);

    assert!(store.consume(&ticket).is_some(), "first consume succeeds");
    assert!(
        store.consume(&ticket).is_none(),
        "a reused ticket must be rejected (single-use)"
    );
}

/// (d) EXPIRY: a ticket presented after its TTL is rejected — and consuming it
/// (even as an expiry-reject) still removes it, so an expired token is inert.
#[test]
fn expired_ticket_is_rejected() {
    let store = WsTicketStore::new();
    let t0 = Instant::now();
    let ticket = store.mint_at(scoped_principal(), None, t0);

    // Just before the TTL edge: still valid.
    let almost = (t0 + WS_TICKET_TTL)
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    // A distinct ticket to probe the edge without consuming the one under test.
    let edge = store.mint_at(scoped_principal(), None, t0);
    assert!(
        store.consume_at(&edge, almost).is_some(),
        "a ticket just inside the TTL is still valid"
    );

    // Past the TTL: rejected.
    let expired = t0 + WS_TICKET_TTL + Duration::from_secs(1);
    assert!(
        store.consume_at(&ticket, expired).is_none(),
        "a ticket past its TTL must be rejected"
    );
}

/// Inv #10: the store is BOUNDED — minting past the capacity drops the oldest
/// rather than growing without limit (the realtime plane can never OOM the host).
#[test]
fn store_is_bounded_drop_oldest() {
    let store = WsTicketStore::new();
    let overshoot = WS_TICKET_CAPACITY + 64;
    let mut tickets = Vec::with_capacity(overshoot);
    for _ in 0..overshoot {
        tickets.push(store.mint(scoped_principal(), None));
    }
    assert!(
        store.len() <= WS_TICKET_CAPACITY,
        "the ticket store never exceeds its capacity ({} > {})",
        store.len(),
        WS_TICKET_CAPACITY
    );
    // The earliest tickets were evicted (drop-oldest); the newest survive.
    assert!(
        store.consume(&tickets[overshoot - 1]).is_some(),
        "the most-recently-minted ticket is retained"
    );
    assert!(
        store.consume(&tickets[0]).is_none(),
        "the oldest ticket was evicted when the cap was exceeded"
    );
}

/// Two distinct mints never collide (high-entropy tokens).
#[test]
fn minted_tickets_are_unique() {
    let store = WsTicketStore::new();
    let a = store.mint(scoped_principal(), None);
    let b = store.mint(scoped_principal(), None);
    assert_ne!(a, b, "each ticket is an independent high-entropy token");
}

/// (a) `POST /api/v1/ws/ticket` REQUIRES authentication and mints a ticket for an
/// authenticated caller.
#[tokio::test]
async fn ticket_endpoint_requires_auth_and_mints() {
    let harness = support::harness();

    // No credential: refused (401), never a ticket.
    let resp = send(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/api/v1/ws/ticket")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "minting a ticket requires authentication"
    );

    // Authenticated: 200 with a non-empty ticket and a positive TTL.
    let resp = send(
        &harness.router,
        post_if_match("/api/v1/ws/ticket", ADMIN_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an authenticated mint succeeds"
    );
    let body = body_json(resp).await;
    assert!(
        body["ticket"].as_str().is_some_and(|t| !t.is_empty()),
        "the response carries a non-empty ticket, got {body}"
    );
    assert!(
        body["expires_in_secs"].as_u64().is_some_and(|s| s > 0),
        "the response advertises a positive TTL, got {body}"
    );
}

/// (e) THE FIX: the durable `Bearer` token is NO LONGER accepted as
/// `?access_token=` on the WS or SSE upgrade — the credential leak is closed.
#[tokio::test]
async fn durable_bearer_query_is_rejected_on_ws_and_sse() {
    let harness = support::harness();

    for path in [
        format!("/api/v1/ws?access_token={ADMIN_TOKEN}"),
        format!("/api/v1/events?access_token={ADMIN_TOKEN}"),
    ] {
        let resp = send(
            &harness.router,
            Request::builder()
                .method("GET")
                .uri(&path)
                // A browser-like same-origin request (so the Origin gate is not
                // what rejects it — the point is the durable bearer is refused).
                .header(header::HOST, "mv.local")
                .header(header::ORIGIN, "http://mv.local")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "the durable bearer in ?access_token= must be rejected on {path}"
        );
    }
}

/// (b/c end-to-end) A minted ticket authenticates the SSE upgrade exactly once:
/// same-origin + `?ticket=` succeeds, and the reused ticket is then rejected.
#[tokio::test]
async fn minted_ticket_authenticates_sse_once() {
    let harness = support::harness();

    // Mint a ticket with the admin bearer (header — never a URL).
    let resp = send(
        &harness.router,
        post_if_match("/api/v1/ws/ticket", ADMIN_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ticket = body_json(resp).await["ticket"]
        .as_str()
        .expect("mint returns a ticket")
        .to_owned();

    // Same-origin SSE with the fresh ticket: accepted (streaming 200).
    let first = send(&harness.router, sse_with_ticket(&ticket)).await;
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "a fresh ticket authenticates the SSE upgrade"
    );
    drop(first); // do not drain the open stream

    // The same ticket again: rejected (single-use consumed on first accept).
    let second = send(&harness.router, sse_with_ticket(&ticket)).await;
    assert_eq!(
        second.status(),
        StatusCode::UNAUTHORIZED,
        "a reused ticket must be rejected on the SSE upgrade"
    );
}

/// A same-origin `GET /api/v1/events?ticket=<t>` request (browser shape).
fn sse_with_ticket(ticket: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/api/v1/events?ticket={ticket}"))
        .header(header::HOST, "mv.local")
        .header(header::ORIGIN, "http://mv.local")
        .body(Body::empty())
        .unwrap()
}
