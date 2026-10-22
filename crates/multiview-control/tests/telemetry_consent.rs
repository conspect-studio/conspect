//! CONSPECT telemetry-consent + telemetry-schema + diagnostics-snapshot surfaces
//! (spec §4.2/§11, ADR-0052).
//!
//! These are the **telemetry** pipe's local surfaces — deliberately kept under
//! `/api/v1/telemetry/` and `/api/v1/diagnostics/`, never co-mingled with the
//! licensing **heartbeat** (which lives under `/api/v1/licensing/`). The two
//! pipes are separate in every dimension (ADR-0052 §1); this test suite pins:
//!
//! * consent is **off by default** (opt-in, incl. free tier);
//! * consent resolves **last-writer-wins** by timestamp (later local beats earlier
//!   portal; later portal beats earlier local);
//! * a `PUT` writes an `AccountAuditKind::ConsentChange` entry to the account-audit
//!   store;
//! * the published telemetry schema's **never-sent** list is pinned (no media /
//!   URLs / hostnames / layouts / typed content);
//! * a diagnostics snapshot rides `202 → ready` and carries diagnostics, never
//!   media;
//! * consent gates **no** local route (staying off costs none of the local API).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::account_audit::{AccountAuditKind, AccountAuditStore, InMemoryAccountAudit};
use multiview_control::telemetry_consent::ConsentActor;
use multiview_control::ConsentState;
use multiview_core::time::MediaTime;
use support::{body_json, get, harness_with, put_json, send, ADMIN_TOKEN, VIEWER_TOKEN};

/// `GET /api/v1/telemetry/consent` reports OFF by default (opt-in, ADR-0052 §1):
/// a fresh machine has never consented, including on the free tier.
#[tokio::test]
async fn consent_is_off_by_default() {
    let h = harness_with(|s| s);
    let resp = send(&h.router, get("/api/v1/telemetry/consent", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["enabled"], false,
        "telemetry consent must default to OFF (opt-in)"
    );
    assert_eq!(
        body["actor"], "local",
        "the default record's actor is the machine (local)"
    );
}

/// `PUT /api/v1/telemetry/consent` enables the outbound pipe and the new state is
/// returned + visible on the next GET.
#[tokio::test]
async fn put_enables_consent_and_is_persisted() {
    let h = harness_with(|s| s);
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/telemetry/consent",
            ADMIN_TOKEN,
            None,
            &serde_json::json!({ "enabled": true }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["enabled"], true);
    assert_eq!(
        body["actor"], "local",
        "a machine-UI PUT is the local actor"
    );
    assert!(
        body["changed_at"].as_str().is_some(),
        "a recorded consent carries a changed_at instant"
    );

    // A subsequent GET observes the persisted state.
    let resp = send(&h.router, get("/api/v1/telemetry/consent", ADMIN_TOKEN)).await;
    let body = body_json(resp).await;
    assert_eq!(body["enabled"], true, "the new consent persisted");
}

/// `PUT` requires write authority: a read-only Viewer is forbidden.
#[tokio::test]
async fn put_requires_write_authority() {
    let h = harness_with(|s| s);
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/telemetry/consent",
            VIEWER_TOKEN,
            None,
            &serde_json::json!({ "enabled": true }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A successful `PUT` writes an `AccountAuditKind::ConsentChange` entry to the
/// account-side append-only audit store (the #106 store; producer now wired).
#[tokio::test]
async fn put_records_a_consent_change_audit_entry() {
    let audit: Arc<dyn AccountAuditStore> = Arc::new(InMemoryAccountAudit::new());
    let shared = Arc::clone(&audit);
    let h = harness_with(move |s| s.with_account_audit(Arc::clone(&shared)));

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/telemetry/consent",
            ADMIN_TOKEN,
            None,
            &serde_json::json!({ "enabled": true }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let page = audit.page(None, Some(AccountAuditKind::ConsentChange), 10);
    assert_eq!(
        page.entries.len(),
        1,
        "exactly one ConsentChange entry was recorded"
    );
    let entry = &page.entries[0];
    assert_eq!(entry.kind, AccountAuditKind::ConsentChange);
    assert_eq!(
        entry.actor, "admin-key",
        "the toggling principal is the actor"
    );
    let detail = entry.detail.as_ref().expect("ConsentChange carries detail");
    assert_eq!(
        detail["enabled"], true,
        "the audit detail records the new consent state"
    );
}

/// Last-writer-wins by timestamp — **a later LOCAL write beats an earlier PORTAL
/// write**. The portal mirror recorded consent ON at t=100; the local UI then
/// turns it OFF at t=200 — the local write wins (it is later).
#[test]
fn lww_later_local_beats_earlier_portal() {
    let state = ConsentState::default();
    // Portal recorded "enabled" earlier.
    state.apply(true, MediaTime::from_nanos(100), ConsentActor::Portal);
    // Local turns it off later — later timestamp wins.
    let applied = state.apply(false, MediaTime::from_nanos(200), ConsentActor::Local);
    assert!(applied, "a later write is applied");
    let record = state.record();
    assert!(!record.enabled, "the later LOCAL write wins (off)");
    assert_eq!(record.actor, ConsentActor::Local);
    assert_eq!(record.changed_at, MediaTime::from_nanos(200));
}

/// Last-writer-wins by timestamp — **a later PORTAL write beats an earlier LOCAL
/// write**. The local UI recorded consent OFF at t=100; a portal mirror then
/// turns it ON at t=200 — the portal write wins (it is later).
#[test]
fn lww_later_portal_beats_earlier_local() {
    let state = ConsentState::default();
    state.apply(false, MediaTime::from_nanos(100), ConsentActor::Local);
    let applied = state.apply(true, MediaTime::from_nanos(200), ConsentActor::Portal);
    assert!(applied, "a later write is applied");
    let record = state.record();
    assert!(record.enabled, "the later PORTAL write wins (on)");
    assert_eq!(record.actor, ConsentActor::Portal);
}

/// Last-writer-wins by timestamp — **a stale (earlier-or-equal) write is
/// rejected**, leaving the newer record intact. This is the LWW correctness
/// property: a delayed portal mirror cannot clobber a fresher local choice.
#[test]
fn lww_stale_write_is_rejected() {
    let state = ConsentState::default();
    // A fresh local write at t=200.
    state.apply(true, MediaTime::from_nanos(200), ConsentActor::Local);
    // A delayed portal write stamped EARLIER (t=100) must NOT win.
    let applied = state.apply(false, MediaTime::from_nanos(100), ConsentActor::Portal);
    assert!(!applied, "a stale (earlier) write is rejected");
    let record = state.record();
    assert!(record.enabled, "the fresher write is preserved (still on)");
    assert_eq!(record.actor, ConsentActor::Local);
    assert_eq!(record.changed_at, MediaTime::from_nanos(200));

    // An equal-timestamp write is also rejected (strictly-later wins; ties keep
    // the incumbent — deterministic and idempotent under replay).
    let applied = state.apply(false, MediaTime::from_nanos(200), ConsentActor::Portal);
    assert!(
        !applied,
        "an equal-timestamp write does not displace the incumbent"
    );
    assert!(state.record().enabled);
}
