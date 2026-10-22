//! SEC-05 (BOLA, ADR-W005/W025/W026): per-object authorization on the preview
//! input surface — the JPEG still `GET /api/v1/preview/inputs/{id}` and the
//! input-id enumeration `GET /api/v1/preview/inputs`.
//!
//! A preview input id is a **source** id (the same object-scope namespace as
//! `GET /inputs/{id}/streams`, which is `authorize_object`-gated). Before this
//! fix the two preview routes checked only the coarse read *role*, so an
//! object-scoped principal could pull ANY input's live still and enumerate every
//! previewable input id — a BOLA read + wholesale enumeration. These tests pin:
//!
//! * the still route 403s an out-of-scope id **before** the provider is consulted
//!   (a spy provider proves zero side effect on denial); an in-scope id and an
//!   unscoped principal are unaffected;
//! * the enumeration is row-filtered to the object scope (a scoped principal sees
//!   only its in-scope ids), a no-op for an unscoped principal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use multiview_control::PreviewProvider;

mod support;
use support::{body_json, get, harness_with, send, OPERATOR_TOKEN, SCOPED_TOKEN};

/// A preview provider that returns a JPEG still for ANY input id and records
/// which ids were queried, so a test can prove per-object authorization runs
/// **before** the provider is consulted (zero side effect on a denial).
struct SpyPreview {
    ids: Vec<String>,
    queried: Mutex<Vec<String>>,
}

impl SpyPreview {
    fn with_ids(ids: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            queried: Mutex::new(Vec::new()),
        })
    }

    /// The input ids the route actually handed to the provider so far.
    fn queried(&self) -> Vec<String> {
        self.queried.lock().expect("queried lock").clone()
    }
}

impl PreviewProvider for SpyPreview {
    fn program_jpeg(&self, _quality: u8) -> Option<Vec<u8>> {
        // A minimal JPEG SOI marker — enough for a 200; content is irrelevant here.
        Some(vec![0xff, 0xd8, 0xff])
    }

    fn input_jpeg(&self, id: &str, _quality: u8) -> Option<Vec<u8>> {
        self.queried
            .lock()
            .expect("queried lock")
            .push(id.to_owned());
        Some(vec![0xff, 0xd8, 0xff])
    }

    fn input_ids(&self) -> Vec<String> {
        self.ids.clone()
    }
}

// `SCOPED_TOKEN` is an operator scoped to the single object id `scoped-layout`.

#[tokio::test]
async fn scoped_principal_is_denied_an_out_of_scope_input_still() {
    let spy = SpyPreview::with_ids(&["cam-1", "scoped-layout"]);
    let probe = Arc::clone(&spy);
    let h = harness_with(move |s| s.with_preview(spy));

    // The scoped operator (object scope = "scoped-layout") asks for cam-1's still.
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an out-of-scope input still must be 403, not the frame"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");

    // ZERO provider side effect: authorization ran BEFORE the provider, so the
    // denied id was never handed to it.
    assert!(
        probe.queried().is_empty(),
        "authorization must run before the preview provider; queried={:?}",
        probe.queried()
    );
}

#[tokio::test]
async fn scoped_principal_may_read_its_in_scope_input_still() {
    let spy = SpyPreview::with_ids(&["scoped-layout"]);
    let probe = Arc::clone(&spy);
    let h = harness_with(move |s| s.with_preview(spy));

    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/scoped-layout.jpg", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the principal's own in-scope input still is allowed"
    );
    assert_eq!(
        probe.queried(),
        vec!["scoped-layout".to_owned()],
        "the in-scope id reached the provider (with `.jpg` stripped)"
    );
}

#[tokio::test]
async fn unscoped_operator_may_read_any_input_still() {
    let spy = SpyPreview::with_ids(&["cam-1"]);
    let h = harness_with(move |s| s.with_preview(spy));

    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unscoped operator reads any input still (the fix is a no-op for it)"
    );
}

#[tokio::test]
async fn list_input_ids_is_filtered_to_the_object_scope() {
    let spy = SpyPreview::with_ids(&["cam-1", "scoped-layout", "cam-2"]);
    let h = harness_with(move |s| s.with_preview(spy));

    // Unscoped operator: the full enumeration, unchanged.
    let all = send(&h.router, get("/api/v1/preview/inputs", OPERATOR_TOKEN)).await;
    assert_eq!(all.status(), StatusCode::OK);
    assert_eq!(
        body_json(all).await,
        serde_json::json!(["cam-1", "scoped-layout", "cam-2"]),
        "an unscoped principal enumerates every previewable input"
    );

    // Scoped operator: only its in-scope input id — no enumeration of the rest.
    let scoped = send(&h.router, get("/api/v1/preview/inputs", SCOPED_TOKEN)).await;
    assert_eq!(scoped.status(), StatusCode::OK);
    assert_eq!(
        body_json(scoped).await,
        serde_json::json!(["scoped-layout"]),
        "a scoped principal sees only its in-scope input id, not the full list"
    );
}
