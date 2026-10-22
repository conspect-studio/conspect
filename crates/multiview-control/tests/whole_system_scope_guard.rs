//! Whole-system-artifact authorization across **every** scope axis (ADR-W026,
//! auth-panel / SEC-10).
//!
//! `require_unscoped_for_whole_system` confines the whole-system desired-state
//! surfaces — `GET /config/export` (embeds every device/`device_ref`/sync-group
//! member id), and the config-state mutations `POST /config/revert-to-start` and
//! `POST /config/promote` (rewrite the ENTIRE running/boot document across all
//! objects) — to a principal that can see the whole system. A principal
//! restricted on ANY axis (object, output, OR discovery-domain) must be denied:
//! a per-field redaction cannot apply to a whole-system document, and a scoped
//! operator has no business rewriting state for objects outside its allowlist.
//!
//! The guard is generalized to the unified `is_global` predicate — NOT a frozen
//! object-only (or object-or-output) check — so an output-only-scoped or a
//! discovery-domain-only-scoped principal cannot bypass it.
//!
//! It also runs FIRST — before the boot-model lookup, idempotency reservation,
//! config composition, command submit, or file write. The positive control
//! proves this: an UNSCOPED operator on `revert`/`promote` (harness has no boot
//! model) reaches the boot-model check and gets `409`, while a scoped operator
//! is stopped at `403` before it — if the guard ran after the boot-model lookup,
//! the scoped operator would also see `409`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use support::{
    body_json, get, harness, post_if_match, send, DISCOVERY_SCOPED_TOKEN, OPERATOR_TOKEN,
    OUTPUT_SCOPED_TOKEN, SCOPED_TOKEN,
};

/// `GET /config/export` for `token`.
fn export(token: &str) -> Request<Body> {
    get("/api/v1/config/export", token)
}

/// `POST /config/revert-to-start` (bodyless, no idempotency key) for `token`.
fn revert(token: &str) -> Request<Body> {
    post_if_match("/api/v1/config/revert-to-start", token, None)
}

/// `POST /config/promote` (bodyless, no idempotency key) for `token`.
fn promote(token: &str) -> Request<Body> {
    post_if_match("/api/v1/config/promote", token, None)
}

/// Send `req` and assert `403` + the RFC 9457 forbidden problem type.
async fn assert_forbidden(router: &Router, req: Request<Body>, ctx: &str) {
    let resp = send(router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "{ctx}: a scoped principal must be denied the whole-system artifact"
    );
    let problem = body_json(resp).await;
    assert_eq!(
        problem["type"], "/problems/forbidden",
        "{ctx}: the denial is an RFC 9457 forbidden problem"
    );
}

/// Send `req` and assert an exact non-403 status (the guard let the principal
/// through to the downstream check — no over-restriction).
async fn assert_status(router: &Router, req: Request<Body>, want: StatusCode, ctx: &str) {
    let resp = send(router, req).await;
    assert_eq!(resp.status(), want, "{ctx}");
}

#[tokio::test]
async fn output_scoped_principal_denied_every_whole_system_surface() {
    // OUTPUT-only scope (scoped_object_ids = None, scoped_output_ids = Some):
    // the object-only guard used to wave this principal straight through.
    let h = harness();
    assert_forbidden(
        &h.router,
        export(OUTPUT_SCOPED_TOKEN),
        "output-scoped export",
    )
    .await;
    assert_forbidden(
        &h.router,
        revert(OUTPUT_SCOPED_TOKEN),
        "output-scoped revert",
    )
    .await;
    assert_forbidden(
        &h.router,
        promote(OUTPUT_SCOPED_TOKEN),
        "output-scoped promote",
    )
    .await;
}

#[tokio::test]
async fn discovery_domain_scoped_principal_denied_every_whole_system_surface() {
    // DISCOVERY-domain-only scope: the new axis W026 adds must be covered by the
    // same generalized predicate, not a two-axis object-or-output check.
    let h = harness();
    assert_forbidden(
        &h.router,
        export(DISCOVERY_SCOPED_TOKEN),
        "discovery-scoped export",
    )
    .await;
    assert_forbidden(
        &h.router,
        revert(DISCOVERY_SCOPED_TOKEN),
        "discovery-scoped revert",
    )
    .await;
    assert_forbidden(
        &h.router,
        promote(DISCOVERY_SCOPED_TOKEN),
        "discovery-scoped promote",
    )
    .await;
}

#[tokio::test]
async fn object_scoped_principal_denied_revert_and_promote() {
    // Regression: `revert`/`promote` omitted the whole-system guard ENTIRELY
    // (only `role.require(Write)`), so even an object-scoped operator could
    // rewrite the entire running/boot config. (Export already gated object
    // scope; these two did not.)
    let h = harness();
    assert_forbidden(&h.router, revert(SCOPED_TOKEN), "object-scoped revert").await;
    assert_forbidden(&h.router, promote(SCOPED_TOKEN), "object-scoped promote").await;
}

#[tokio::test]
async fn unscoped_operator_passes_the_guard_before_the_boot_model_lookup() {
    // The guard does NOT over-restrict an unscoped principal, AND it runs before
    // the boot-model lookup: the harness has no boot model, so an unscoped
    // operator reaches that check and gets 409 on revert/promote (not 403), and
    // 422 on export (no working layout). A scoped operator, by contrast, is
    // stopped at 403 *before* the boot-model lookup — proving ordering.
    let h = harness();
    assert_status(
        &h.router,
        revert(OPERATOR_TOKEN),
        StatusCode::CONFLICT,
        "unscoped revert reaches the boot-model check (no model → 409, not 403)",
    )
    .await;
    assert_status(
        &h.router,
        promote(OPERATOR_TOKEN),
        StatusCode::CONFLICT,
        "unscoped promote reaches the boot-model check (no model → 409, not 403)",
    )
    .await;
    assert_status(
        &h.router,
        export(OPERATOR_TOKEN),
        StatusCode::UNPROCESSABLE_ENTITY,
        "unscoped export reaches composition (no working layout → 422, not 403)",
    )
    .await;
}
