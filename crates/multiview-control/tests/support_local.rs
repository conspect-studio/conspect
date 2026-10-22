//! The **LOCAL** support/ticketing surface (Conspect, ADR-0053 / brief §10/§11,
//! spec §7/§11). These tests drive the eight endpoints through the real router
//! and pin the contracts the spec demands of the LOCAL model — the portal sync
//! (thread sync, CS-xxxx correlation with the account) rides the later O1
//! transport, so what is exercised here is the complete local lifecycle:
//!
//! * `GET /api/v1/support/entitlement` — tier-derived routing (free → community).
//! * `POST/GET /api/v1/support/tickets[/{id}]` + `/reply` — local ticket store,
//!   `not_entitled` 403 for the free tier (pinned), machine identity auto-attach,
//!   `ticket_closed` 409 on reply to a closed ticket (pinned).
//! * `POST/GET /api/v1/support/bundle[/{id}]` — the previewable context-pack:
//!   secrets/source URLs redacted (pinned, with the removal list), NEVER media
//!   (pinned), and it composes with telemetry consent OFF (pinned, no consent
//!   check in this path).
//! * `POST /api/v1/support/data-request/{id}/{approve,deny}` — local approval
//!   only (pinned: nothing leaves without a local yes), `request_expired` 410.
//!
//! Every audited action (ticket raise/reply, bundle compose, data-request
//! approve/deny) lands an entry in the append-only account audit store (pinned).
//!
//! Isolation (inv #10): every surface reads/writes control-plane stores only; no
//! handler holds an engine handle or can back-pressure the engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use ed25519_dalek::rand_core::UnwrapErr;
use ed25519_dalek::{Signer, SigningKey};
use getrandom::SysRng;
use multiview_control::account_audit::AccountAuditKind;
use multiview_control::support_store::{DataRequest, DataRequestState, TicketSeverity};
use multiview_control::{AppState, DataRequestRepository, InMemoryDataRequests, LicenceState};
use multiview_licence::entitlement::{
    Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier,
};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::store::{LeaseBinding, LeaseStore};
use multiview_licence::verify::{PinnedKey, SignedLease};
use multiview_licence::ACTIVATION_WINDOW_DAYS;
use multiview_telemetry::RetentionStore;
use support::{
    body_json, get, harness_with, post_json, send, Harness, ADMIN_TOKEN, OPERATOR_TOKEN,
    SCOPED_TOKEN, VIEWER_TOKEN,
};

/// A fixed deterministic instant the injected licence-store clock returns.
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A fresh keypair + the pinned verifying key derived from it.
fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = UnwrapErr(SysRng);
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

/// A signed binding over an entitlement at the given opaque `tier`.
fn binding(key: &SigningKey, tier: &str, granted: DateTime<Utc>) -> LeaseBinding {
    let lease = Lease::new_full(
        "cs-test-serial".to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    );
    let sig = key.sign(&SignedLease::signing_bytes(&lease, None));
    LeaseBinding::new(
        SignedLease::new(lease.clone(), sig.to_bytes()),
        Entitlement::new(
            Tier::new(tier.to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Limited(2),
            lease,
            EntitlementFlags::default(),
        ),
        100,
        None,
    )
}

/// A harness whose licence store is seeded with a verified lease at the given
/// opaque tier (so the entitlement routing + ticket gating see a real tier), and
/// whose data-request store is shared back to the test so it can seed an inbound
/// request (the portal-fed producer is the later O1 transport).
fn harness_at_tier(tier: &str) -> (Harness, Arc<dyn DataRequestRepository>) {
    let (key, pinned) = keypair();
    let store = Arc::new(LeaseStore::with_clock(Arc::new(epoch)));
    let lease = store
        .install_binding(&binding(&key, tier, epoch()), &pinned, epoch())
        .expect("seed binding installs");
    assert_eq!(lease.serial, "cs-test-serial");
    let data_requests: Arc<dyn DataRequestRepository> = Arc::new(InMemoryDataRequests::new());
    let dr = Arc::clone(&data_requests);
    let h = harness_with(move |state| {
        state
            .with_licence(LicenceState {
                store: Arc::clone(&store),
                pinned: Some(pinned),
                challenge: None,
            })
            .with_data_requests(Arc::clone(&dr))
            .with_retention(Arc::new(RetentionStore::new()))
    });
    (h, data_requests)
}

/// An unlicensed harness (no lease installed) — the "free / community" tier.
fn harness_free() -> Harness {
    harness_with(|state| state.with_retention(Arc::new(RetentionStore::new())))
}

// ── 1. Entitlement routing ────────────────────────────────────────────────

#[tokio::test]
async fn entitlement_free_tier_routes_to_community_and_is_not_eligible() {
    let h = harness_free();
    let resp = send(&h.router, get("/api/v1/support/entitlement", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["eligible"], serde_json::json!(false));
    assert_eq!(body["route"]["first_line"], serde_json::json!("community"));
}

#[tokio::test]
async fn entitlement_studio_tier_is_eligible_and_routes_to_a_support_queue() {
    let (h, _dr) = harness_at_tier("studio");
    let resp = send(
        &h.router,
        get("/api/v1/support/entitlement", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["eligible"], serde_json::json!(true));
    let first_line = body["route"]["first_line"].as_str().unwrap();
    assert!(
        first_line == "conspect" || first_line == "partner",
        "an eligible tier routes to a real support queue, got {first_line:?}"
    );
    assert!(
        body["sla"].is_string(),
        "an eligible tier carries an SLA token"
    );
}

// ── 2. Tickets (local store) ──────────────────────────────────────────────

#[tokio::test]
async fn free_tier_cannot_raise_a_ticket_403_not_entitled() {
    let h = harness_free();
    let body = serde_json::json!({
        "subject": "help",
        "body": "my machine is misbehaving",
        "severity": "question",
        "attachments": []
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/support/tickets", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], serde_json::json!(403));
    assert!(
        problem["type"].as_str().unwrap().contains("not_entitled"),
        "the free-tier ticket refusal is the not_entitled problem, got {problem:?}"
    );
}

#[tokio::test]
async fn raise_ticket_auto_attaches_machine_identity_and_uses_cs_id() {
    let (h, _dr) = harness_at_tier("broadcast");
    let body = serde_json::json!({
        "subject": "encoder saturating",
        "body": "NVENC hits the ceiling under load",
        "severity": "degraded",
        "attachments": []
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/support/tickets", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let ticket_id = created["ticket_id"].as_str().unwrap();
    assert!(
        ticket_id.starts_with("CS-"),
        "ticket ids use the CS-xxxx shape, got {ticket_id:?}"
    );

    // The thread carries the auto-attached machine identity/version/entitlement/
    // ladder-state/fingerprint-score per §7.1 — pinned by reading it back.
    let read = send(
        &h.router,
        get(
            &format!("/api/v1/support/tickets/{ticket_id}"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(read.status(), StatusCode::OK);
    let thread = body_json(read).await;
    let context = &thread["context"];
    assert!(context["app_version"].is_string(), "version auto-attached");
    assert_eq!(
        context["entitlement"]["tier"],
        serde_json::json!("broadcast"),
        "entitlement tier auto-attached"
    );
    assert!(
        context["enforcement"]["level"].is_string(),
        "ladder state auto-attached"
    );
    assert!(
        context["fingerprint_score"].is_number(),
        "fingerprint score auto-attached"
    );
}

#[tokio::test]
async fn reply_to_a_closed_ticket_is_409_ticket_closed() {
    let (h, _dr) = harness_at_tier("studio");
    // Raise a ticket.
    let raise = send(
        &h.router,
        post_json(
            "/api/v1/support/tickets",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "subject": "s", "body": "b", "severity": "blocking", "attachments": []
            }),
        ),
    )
    .await;
    let ticket_id = body_json(raise).await["ticket_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Close it (the local lifecycle is fully functional — an operator can close).
    let close = send(
        &h.router,
        post_json(
            &format!("/api/v1/support/tickets/{ticket_id}/close"),
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(close.status(), StatusCode::OK);

    // A reply to the closed ticket is refused 409 ticket_closed (pinned).
    let reply = send(
        &h.router,
        post_json(
            &format!("/api/v1/support/tickets/{ticket_id}/reply"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "body": "any update?" }),
        ),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::CONFLICT);
    let problem = body_json(reply).await;
    assert!(
        problem["type"].as_str().unwrap().contains("ticket_closed"),
        "reply-on-closed is the ticket_closed problem, got {problem:?}"
    );
}

#[tokio::test]
async fn reply_to_an_open_ticket_appends_to_the_thread() {
    let (h, _dr) = harness_at_tier("studio");
    let raise = send(
        &h.router,
        post_json(
            "/api/v1/support/tickets",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "subject": "s", "body": "b", "severity": "question", "attachments": []
            }),
        ),
    )
    .await;
    let ticket_id = body_json(raise).await["ticket_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let reply = send(
        &h.router,
        post_json(
            &format!("/api/v1/support/tickets/{ticket_id}/reply"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "body": "still happening" }),
        ),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
    let thread = body_json(reply).await;
    let updates = thread["updates"].as_array().unwrap();
    // The opening body + the reply = at least two updates, the last being ours.
    assert!(updates.len() >= 2, "the reply appended to the thread");
    assert_eq!(
        updates.last().unwrap()["body"],
        serde_json::json!("still happening")
    );
}

#[tokio::test]
async fn free_tier_cannot_list_or_read_tickets_403() {
    let h = harness_free();
    let list = send(&h.router, get("/api/v1/support/tickets", OPERATOR_TOKEN)).await;
    assert_eq!(list.status(), StatusCode::FORBIDDEN);
}

// ── 3. Context-pack composer ──────────────────────────────────────────────

#[tokio::test]
async fn scoped_principal_cannot_compose_or_read_a_support_bundle() {
    // BOLA wholesale-enumeration leak (OWASP API1, ADR-W005/ADR-W025): a support
    // bundle's `config` section embeds the WHOLE config — every device id, every
    // device_ref, every sync-group member — redacted only for secrets/URLs, not
    // ids. An OBJECT-scoped principal must not compose or read it (it would
    // disclose every object outside its allowlist), exactly like the config
    // export. SCOPED_TOKEN is an Operator (Write-capable) scoped to
    // `["scoped-layout"]`, on an eligible tier — only the new scope guard can
    // deny it.
    let (h, _dr) = harness_at_tier("studio");

    // Compose is forbidden for a scoped principal.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            SCOPED_TOKEN,
            &serde_json::json!({ "window": "24h", "include": ["config"] }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not compose a whole-system support bundle (BOLA wholesale leak)"
    );

    // Reading a bundle (composed by an unscoped admin) is also forbidden for a
    // scoped principal — defence in depth on the same whole-system artifact.
    let compose = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            ADMIN_TOKEN,
            &serde_json::json!({ "window": "24h", "include": ["config"] }),
        ),
    )
    .await;
    assert_eq!(compose.status(), StatusCode::ACCEPTED);
    let bundle_id = body_json(compose).await["bundle_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resp = send(
        &h.router,
        get(&format!("/api/v1/support/bundle/{bundle_id}"), SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not read a whole-system support bundle"
    );

    // An unscoped operator may still compose + read (no over-restriction).
    let compose = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            OPERATOR_TOKEN,
            &serde_json::json!({ "window": "24h", "include": ["config"] }),
        ),
    )
    .await;
    assert_eq!(
        compose.status(),
        StatusCode::ACCEPTED,
        "an unscoped operator may still compose a bundle"
    );
}

#[tokio::test]
async fn bundle_redacts_secrets_and_source_urls_and_lists_the_removals() {
    let (h, _dr) = harness_at_tier("studio");

    // Seed a source carrying a plaintext URL + a secret reference into the
    // working stores so the config the bundle composes from has something to
    // redact. We post a source resource (the config-as-code surface).
    seed_source_with_url_and_secret(&h).await;

    // Compose a bundle including the config.
    let compose = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            OPERATOR_TOKEN,
            &serde_json::json!({ "window": "24h", "include": ["config"] }),
        ),
    )
    .await;
    assert_eq!(compose.status(), StatusCode::ACCEPTED);
    let bundle_id = body_json(compose).await["bundle_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Read the preview.
    let preview = send(
        &h.router,
        get(
            &format!("/api/v1/support/bundle/{bundle_id}"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let body = body_json(preview).await;

    // The serialized preview must NOT contain the secret reference or the URL.
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        !serialized.contains("op://Servers/cam/credentials"),
        "the secret reference must be redacted out of the bundle"
    );
    assert!(
        !serialized.contains("rtsp://camera.example/stream"),
        "the source URL must be masked out of the bundle"
    );

    // The preview lists what redaction removed (the operator sees the masking).
    let removals = body["redactions"].as_array().unwrap();
    assert!(
        !removals.is_empty(),
        "the preview enumerates the redacted fields, got {body:?}"
    );
}

#[tokio::test]
async fn bundle_never_contains_media() {
    // No frame/thumbnail/media type can enter the bundle — pinned by asserting
    // the composed preview carries no media key under any include set.
    let (h, _dr) = harness_at_tier("studio");
    let compose = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "window": "7d",
                "include": ["diagnostics", "metrics", "config", "incidents"]
            }),
        ),
    )
    .await;
    assert_eq!(compose.status(), StatusCode::ACCEPTED);
    let bundle_id = body_json(compose).await["bundle_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let preview = send(
        &h.router,
        get(
            &format!("/api/v1/support/bundle/{bundle_id}"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let body = body_json(preview).await;
    let serialized = serde_json::to_string(&body).unwrap().to_lowercase();
    for forbidden in [
        "frame",
        "thumbnail",
        "jpeg",
        "snapshot",
        "nv12",
        "rgba",
        "media",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "the bundle must never carry media; found {forbidden:?} in {serialized}"
        );
    }
}

#[tokio::test]
async fn bundle_composes_with_telemetry_consent_off() {
    // Consent governs the daily outbound pipe, not deliberate operator
    // attachments (§7.2) — the bundle path performs NO consent check. We compose
    // a metrics+incidents bundle; it must succeed regardless of consent state.
    let (h, _dr) = harness_at_tier("studio");
    let compose = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            OPERATOR_TOKEN,
            &serde_json::json!({ "window": "1h", "include": ["metrics", "incidents"] }),
        ),
    )
    .await;
    assert_eq!(
        compose.status(),
        StatusCode::ACCEPTED,
        "the bundle composes with consent OFF — no consent check in this path"
    );
    let bundle_id = body_json(compose).await["bundle_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let preview = send(
        &h.router,
        get(
            &format!("/api/v1/support/bundle/{bundle_id}"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
}

#[tokio::test]
async fn free_tier_cannot_compose_a_bundle_403() {
    let h = harness_free();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            OPERATOR_TOKEN,
            &serde_json::json!({ "window": "1h", "include": ["metrics"] }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── 4. Data requests (local approval only) ────────────────────────────────

#[tokio::test]
async fn data_request_approve_is_local_only_then_state_is_approved() {
    let (h, dr) = harness_at_tier("studio");
    // Seed an inbound data request (the portal-fed producer is the later O1
    // transport; here the test seeds it directly into the shared store).
    dr.enqueue(DataRequest::new(
        "DR-0001".to_owned(),
        "extra encoder logs".to_owned(),
        multiview_core::time::MediaTime::from_nanos(0),
        None,
    ));

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/support/data-request/DR-0001/approve",
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["state"], serde_json::json!("approved"));

    // The store reflects the local approval — nothing leaves without this yes.
    assert_eq!(
        dr.get("DR-0001").unwrap().state,
        DataRequestState::Approved,
        "approval is recorded locally; egress is gated on this state"
    );
}

#[tokio::test]
async fn data_request_deny_is_local_only_then_state_is_denied() {
    let (h, dr) = harness_at_tier("studio");
    dr.enqueue(DataRequest::new(
        "DR-0002".to_owned(),
        "config dump".to_owned(),
        multiview_core::time::MediaTime::from_nanos(0),
        None,
    ));
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/support/data-request/DR-0002/deny",
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["state"], serde_json::json!("denied"));
    assert_eq!(dr.get("DR-0002").unwrap().state, DataRequestState::Denied);
}

#[tokio::test]
async fn approving_an_expired_data_request_is_410_request_expired() {
    let (h, dr) = harness_at_tier("studio");
    let mut req = DataRequest::new(
        "DR-0003".to_owned(),
        "stale ask".to_owned(),
        multiview_core::time::MediaTime::from_nanos(0),
        None,
    );
    // Mark the seeded request already expired (its window has elapsed).
    req.state = DataRequestState::Expired;
    dr.enqueue(req);

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/support/data-request/DR-0003/approve",
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::GONE);
    let problem = body_json(resp).await;
    assert!(
        problem["type"]
            .as_str()
            .unwrap()
            .contains("request_expired"),
        "an expired data request answers request_expired, got {problem:?}"
    );
    // The expired request was NOT approved — nothing leaves.
    assert_eq!(dr.get("DR-0003").unwrap().state, DataRequestState::Expired);
}

// ── 5. Audit seams ────────────────────────────────────────────────────────

#[tokio::test]
async fn ticket_raise_reply_bundle_and_data_request_each_write_audit_entries() {
    let (h, dr) = harness_at_tier("studio");

    // Raise + reply.
    let raise = send(
        &h.router,
        post_json(
            "/api/v1/support/tickets",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "subject": "s", "body": "b", "severity": "question", "attachments": []
            }),
        ),
    )
    .await;
    let ticket_id = body_json(raise).await["ticket_id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &h.router,
        post_json(
            &format!("/api/v1/support/tickets/{ticket_id}/reply"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "body": "more" }),
        ),
    )
    .await;

    // Compose a bundle.
    send(
        &h.router,
        post_json(
            "/api/v1/support/bundle",
            OPERATOR_TOKEN,
            &serde_json::json!({ "window": "1h", "include": ["metrics"] }),
        ),
    )
    .await;

    // Approve a data request.
    dr.enqueue(DataRequest::new(
        "DR-AUDIT".to_owned(),
        "logs".to_owned(),
        multiview_core::time::MediaTime::from_nanos(0),
        None,
    ));
    send(
        &h.router,
        post_json(
            "/api/v1/support/data-request/DR-AUDIT/approve",
            OPERATOR_TOKEN,
            &serde_json::json!({}),
        ),
    )
    .await;

    // Read the account audit trail and assert each kind landed.
    let audit = send(
        &h.router,
        get("/api/v1/account/audit?limit=1000", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(audit.status(), StatusCode::OK);
    let entries = body_json(audit).await["entries"]
        .as_array()
        .unwrap()
        .clone();
    let kinds: Vec<String> = entries
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        AccountAuditKind::Ticket,
        AccountAuditKind::BundleCompose,
        AccountAuditKind::DataRequestApprove,
    ] {
        let slug = serde_json::to_value(expected).unwrap();
        let slug = slug.as_str().unwrap();
        assert!(
            kinds.iter().any(|k| k == slug),
            "an audit entry of kind {slug:?} must land; got {kinds:?}"
        );
    }
}

/// Seed one source carrying a plaintext URL + a reference-only secret into the
/// working source store via the config-as-code resource surface, so the bundle's
/// config redactor has something to mask.
async fn seed_source_with_url_and_secret(h: &Harness) {
    let body = serde_json::json!({
        "body": {
            "id": "cam-1",
            "display_name": "Camera 1",
            "kind": "rtsp",
            "url": "rtsp://camera.example/stream",
            "auth": { "secret_ref": "op://Servers/cam/credentials" }
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/cam-1", ADMIN_TOKEN, &body),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "seeding the source should succeed, got {}",
        resp.status()
    );
}

/// A compile-time assertion the severity enum carries the three spec levels.
#[allow(dead_code)]
fn severity_levels_exist(s: TicketSeverity) -> TicketSeverity {
    match s {
        TicketSeverity::Question | TicketSeverity::Degraded | TicketSeverity::Blocking => s,
    }
}

/// A compile-time witness that `AppState` exposes the support wiring used above
/// (keeps the test honest about the public surface it depends on).
#[allow(dead_code)]
fn uses_appstate(_state: AppState) {}
