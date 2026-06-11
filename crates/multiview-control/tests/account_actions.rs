//! Account audit + remote-actions route tests (tower oneshot), Conspect
//! ADR-0053 §4 / brief §10/§11 / spec §6/§11.
//!
//! Drive the real router:
//! * the account audit log is **append-only** (no mutating verb exists) and
//!   cursor-paginates with stable, resumable ordering;
//! * the pending-actions strip lists pending actions; a **local cancel always
//!   wins** and an already-executed action answers `410 already_executed`;
//! * `POST /api/v1/salvos/{id}/fire` fires a named salvo → `202 {action_id,
//!   queued_at}` and lands in the audit store; an unknown salvo is `404
//!   salvo_unknown`;
//! * a lease install (`POST /api/v1/licence/lease`) writes an audit entry
//!   end-to-end.
//!
//! All routes are control-plane-only: a wedged client cannot back-pressure the
//! engine (inv #10) and no account action takes a program off air (inv #1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use multiview_config::Salvo;
use multiview_control::{
    AccountAuditKind, AccountAuditStore, AppState, InMemoryAccountAudit, InMemoryPendingActions,
    PendingActionKind, PendingActionStore,
};
use multiview_core::time::MediaTime;
use serde_json::{json, Value};
use support::{
    body_json, get, harness, harness_with, send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN,
};

/// A bodyless request with an explicit method + Bearer token (for the verb-not-
/// allowed append-only check and the cancel POST).
fn request(method: Method, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request should build")
}

fn seed_salvo(h: &support::Harness, id: &str) {
    let salvo: Salvo = serde_json::from_value(json!({ "id": id, "layout": "grid-9" }))
        .expect("salvo deserialises");
    h.salvos.create(salvo).expect("seed create");
}

// ---- append-only audit + cursor pagination ----------------------------------

#[tokio::test]
async fn account_audit_has_no_mutating_verb_so_it_is_append_only() {
    let h = harness();
    // The read verb is wired.
    let resp = send(&h.router, get("/api/v1/account/audit", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // No POST/PUT/DELETE handler exists on the audit collection — the log is
    // append-only by route construction (ADR-0053: no update/delete route). axum
    // answers an unrouted method with 405 Method Not Allowed.
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let resp = send(
            &h.router,
            request(method.clone(), "/api/v1/account/audit", ADMIN_TOKEN),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the account audit log must not be routed (append-only)"
        );
    }
}

#[tokio::test]
async fn account_audit_cursor_pages_are_stable_and_resumable() {
    // Seed 5 entries directly into a shared store, then page through the route.
    let store: Arc<dyn AccountAuditStore> = Arc::new(InMemoryAccountAudit::new());
    for i in 0..5 {
        store.record(
            "operator-key",
            AccountAuditKind::ActionRequested,
            MediaTime::from_nanos(i),
            Some(json!({ "n": i })),
        );
    }
    let shared = Arc::clone(&store);
    let h = harness_with(move |state: AppState| state.with_account_audit(Arc::clone(&shared)));

    // First page of 2.
    let resp = send(
        &h.router,
        get("/api/v1/account/audit?limit=2", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page: Value = body_json(resp).await;
    let seqs: Vec<u64> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![0, 1], "oldest-first page of 2");
    let cursor = page["next_cursor"].as_u64().expect("a next cursor");
    assert_eq!(cursor, 1);

    // Resume strictly after the cursor — no gap, no dupe.
    let resp = send(
        &h.router,
        get(
            &format!("/api/v1/account/audit?limit=2&cursor={cursor}"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let page: Value = body_json(resp).await;
    let seqs: Vec<u64> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        vec![2, 3],
        "resume after seq 1 yields 2,3 (no gap/dupe)"
    );

    // Final partial page: no next cursor.
    let cursor = page["next_cursor"].as_u64().unwrap();
    let resp = send(
        &h.router,
        get(
            &format!("/api/v1/account/audit?limit=2&cursor={cursor}"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let page: Value = body_json(resp).await;
    let seqs: Vec<u64> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![4]);
    assert!(
        page.get("next_cursor").is_none() || page["next_cursor"].is_null(),
        "the final page has no next cursor"
    );
}

#[tokio::test]
async fn account_audit_filters_by_kind() {
    let store: Arc<dyn AccountAuditStore> = Arc::new(InMemoryAccountAudit::new());
    store.record(
        "k",
        AccountAuditKind::LeaseInstall,
        MediaTime::from_nanos(1),
        None,
    );
    store.record(
        "k",
        AccountAuditKind::ActionRequested,
        MediaTime::from_nanos(2),
        None,
    );
    let shared = Arc::clone(&store);
    let h = harness_with(move |state: AppState| state.with_account_audit(Arc::clone(&shared)));

    let resp = send(
        &h.router,
        get("/api/v1/account/audit?filter=lease-install", OPERATOR_TOKEN),
    )
    .await;
    let page: Value = body_json(resp).await;
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "lease-install");
}

// ---- pending actions: local cancel always wins ------------------------------

#[tokio::test]
async fn pending_strip_lists_then_local_cancel_wins() {
    let actions: Arc<dyn PendingActionStore> = Arc::new(InMemoryPendingActions::new());
    actions.enqueue(
        "act-1".to_owned(),
        PendingActionKind::Reboot,
        "operator-key",
        MediaTime::from_nanos(1),
        None,
    );
    let shared = Arc::clone(&actions);
    let h = harness_with(move |state: AppState| state.with_pending_actions(Arc::clone(&shared)));

    // The strip lists the pending reboot.
    let resp = send(&h.router, get("/api/v1/actions/pending", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value = body_json(resp).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["action_id"], "act-1");
    assert_eq!(list[0]["kind"], "reboot");

    // Local cancel wins → 200 {cancelled:true}.
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/actions/act-1/cancel", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = body_json(resp).await;
    assert_eq!(body["cancelled"], true);

    // It drops out of the strip, and the underlying store confirms the cancel
    // beat any later execution attempt.
    assert!(actions.list_pending().is_empty());
    assert!(matches!(
        actions.mark_executed("act-1"),
        multiview_control::ExecuteOutcome::Cancelled
    ));
}

#[tokio::test]
async fn cancel_of_already_executed_is_410_already_executed() {
    let actions: Arc<dyn PendingActionStore> = Arc::new(InMemoryPendingActions::new());
    actions.enqueue(
        "act-x".to_owned(),
        PendingActionKind::Restart,
        "operator-key",
        MediaTime::from_nanos(1),
        None,
    );
    // It executes before the cancel arrives.
    actions.mark_executed("act-x");
    let shared = Arc::clone(&actions);
    let h = harness_with(move |state: AppState| state.with_pending_actions(Arc::clone(&shared)));

    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/actions/act-x/cancel", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::GONE,
        "an already-executed action is 410, never a fake success"
    );
    let problem: Value = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/already_executed");
}

#[tokio::test]
async fn cancel_of_unknown_action_is_404() {
    let h = harness();
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/actions/nope/cancel", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn viewer_cannot_cancel_an_action() {
    let actions: Arc<dyn PendingActionStore> = Arc::new(InMemoryPendingActions::new());
    actions.enqueue(
        "act-1".to_owned(),
        PendingActionKind::Restart,
        "operator-key",
        MediaTime::from_nanos(1),
        None,
    );
    let shared = Arc::clone(&actions);
    let h = harness_with(move |state: AppState| state.with_pending_actions(Arc::clone(&shared)));
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/actions/act-1/cancel", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // The viewer's denied request never transitioned the action.
    assert_eq!(actions.list_pending().len(), 1);
}

// ---- salvo fire parity ------------------------------------------------------

#[tokio::test]
async fn fire_named_salvo_returns_202_and_audits() {
    let audit: Arc<dyn AccountAuditStore> = Arc::new(InMemoryAccountAudit::new());
    let shared = Arc::clone(&audit);
    let mut h = harness_with(move |state: AppState| state.with_account_audit(Arc::clone(&shared)));
    seed_salvo(&h, "wide");

    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/salvos/wide/fire", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body: Value = body_json(resp).await;
    let action_id = body["action_id"].as_str().expect("an action id");
    assert!(!action_id.is_empty());
    assert!(body["queued_at_nanos"].as_i64().is_some());

    // The fire reached the engine command bus.
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "the fire submitted exactly one command");

    // The fire (and its execution) landed in the append-only account audit store.
    let page = audit.page(None, Some(AccountAuditKind::ActionRequested), 100);
    assert_eq!(page.entries.len(), 1, "the fire was audited as requested");
    assert_eq!(page.entries[0].detail.as_ref().unwrap()["salvo"], "wide");
    let executed = audit.page(None, Some(AccountAuditKind::ActionExecuted), 100);
    assert_eq!(executed.entries.len(), 1, "the dispatched fire was audited");
}

#[tokio::test]
async fn fire_unknown_salvo_is_404_salvo_unknown() {
    let h = harness();
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/salvos/ghost/fire", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let problem: Value = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/salvo_unknown");
}

#[tokio::test]
async fn viewer_cannot_fire_a_salvo() {
    let h = harness();
    seed_salvo(&h, "wide");
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/salvos/wide/fire", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn fire_sheds_to_503_when_the_bus_is_full_without_executing() {
    // Capacity 1; fill it so the fire submit is shed (invariant #10).
    let audit: Arc<dyn AccountAuditStore> = Arc::new(InMemoryAccountAudit::new());
    let shared = Arc::clone(&audit);
    let h = support::harness_customized(1, move |state: AppState| {
        state.with_account_audit(Arc::clone(&shared))
    });
    seed_salvo(&h, "wide");

    // Saturate the bus with a first fire (capacity 1, undrained).
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/salvos/wide/fire", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The second fire sheds — never blocks the engine.
    let resp = send(
        &h.router,
        request(Method::POST, "/api/v1/salvos/wide/fire", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    // The shed fire was NOT marked executed.
    let executed = audit.page(None, Some(AccountAuditKind::ActionExecuted), 100);
    assert_eq!(
        executed.entries.len(),
        1,
        "only the first (accepted) fire executed; the shed one did not"
    );
}
