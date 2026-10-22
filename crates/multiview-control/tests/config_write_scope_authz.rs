//! SEC-10 (BFLA — broken function-level authz, CWE-285 / OWASP API5:2023): the
//! whole-system config-WRITE endpoints `POST /api/v1/config/revert-to-start` and
//! `POST /api/v1/config/promote` rewrite / promote the ENTIRE running
//! configuration. Like `GET /api/v1/config/export` (which already calls
//! `require_unscoped_for_whole_system`), they must be confined to an UNSCOPED
//! principal — otherwise an object-scoped Operator, authorized for only a subset
//! of objects, could revert or promote the whole system.
//!
//! These tests use `support::harness()`, which wires no boot model, so an
//! authorized (unscoped) caller reaches the boot-model check and gets `409`.
//! That is exactly what proves the whole-system guard does not over-restrict: an
//! unscoped Operator passes the guard (409, not 403), while an object-scoped
//! Operator is stopped AT the guard (403). The guard is evaluated before the
//! boot-model existence check, so an unauthorized principal never learns whether
//! a boot model exists.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use support::{body_json, harness, post_if_match, send, OPERATOR_TOKEN, SCOPED_TOKEN};

#[tokio::test]
async fn scoped_operator_cannot_revert_to_start() {
    let h = harness();

    // An OBJECT-scoped operator (`SCOPED_TOKEN`, allowlist `["scoped-layout"]`)
    // must be forbidden the whole-system revert.
    let resp = send(
        &h.router,
        post_if_match("/api/v1/config/revert-to-start", SCOPED_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an object-scoped operator must not revert the whole running config (BFLA)"
    );
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");

    // An UNSCOPED operator passes the whole-system guard and reaches the
    // boot-model check — 409 (this run has no boot model), NOT 403. This proves
    // the guard does not over-restrict a legitimately-unscoped principal.
    let resp = send(
        &h.router,
        post_if_match("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "an unscoped operator passes the whole-system guard (409: no boot model in this run)"
    );
}

#[tokio::test]
async fn scoped_operator_cannot_promote_to_boot() {
    let h = harness();

    let resp = send(
        &h.router,
        post_if_match("/api/v1/config/promote", SCOPED_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an object-scoped operator must not promote the whole running config to boot (BFLA)"
    );
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");

    let resp = send(
        &h.router,
        post_if_match("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "an unscoped operator passes the whole-system guard (409: no boot model in this run)"
    );
}
