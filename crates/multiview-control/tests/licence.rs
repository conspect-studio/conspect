//! Local-licence control-route tests (Conspect, CONSPECT-1 / ADR-0050).
//!
//! Drive the three endpoints through the real router:
//! * install a verified binding via `POST /api/v1/licence/lease`, then confirm
//!   `GET /api/v1/licence` renders the installed tier + the computed ladder
//!   `state`/`enforcement` (enforcement is DATA → `200`, never `5xx`);
//! * a tampered lease is rejected `422 signature_invalid` (RFC-9457 problem);
//! * the challenge export is well-formed `application/cbor`.
//!
//! The licence plane is control-plane-only data: these routes read a store and
//! verify signatures; they hold no engine handle and cannot take a running
//! program off air (inv #1) or back-pressure the engine (inv #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use multiview_control::LicenceState;
use multiview_licence::entitlement::{
    Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier,
};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::store::{LeaseBinding, LeaseStore};
use multiview_licence::verify::{PinnedKey, SignedLease};
use multiview_licence::ACTIVATION_WINDOW_DAYS;
use rand_core::OsRng;
use support::{body_bytes, body_json, harness_with, send, ADMIN_TOKEN, VIEWER_TOKEN};

/// A fixed, deterministic instant the injected store clock returns.
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A fresh keypair + the pinned verifying key derived from it.
fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = OsRng;
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

/// A signed binding over a `studio`/`Limited(2)` entitlement at `granted`, with
/// the given fingerprint `score`.
fn binding(key: &SigningKey, serial: &str, granted: DateTime<Utc>, score: u8) -> LeaseBinding {
    let lease = Lease::new_full(
        serial.to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    );
    let sig = key.sign(&SignedLease::signing_bytes(&lease));
    LeaseBinding::new(
        SignedLease::new(lease.clone(), sig.to_bytes()),
        Entitlement::new(
            Tier::new("studio".to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Limited(2),
            lease,
            EntitlementFlags::default(),
        ),
        score,
    )
}

/// Build a licence state pinned to `pinned`, with the store's clock fixed at
/// `now` so the computed ladder state is deterministic.
fn licence_state(pinned: PinnedKey, now: DateTime<Utc>) -> LicenceState {
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    LicenceState::new(store, Some(pinned))
}

/// A `POST` with a Bearer token and a raw `application/cbor` body.
fn post_cbor(path: &str, token: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/cbor")
        .body(Body::from(body))
        .expect("request should build")
}

#[tokio::test]
async fn install_then_get_shows_tier_and_computed_ladder_state() {
    let (key, pinned) = keypair();
    let now = epoch();
    let h = harness_with(|state| state.with_licence(licence_state(pinned, now)));

    // Before any install the resource is an honest `licensed: false` 200.
    let resp = send(&h.router, support::get("/api/v1/licence", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let pre = body_json(resp).await;
    assert_eq!(pre["licensed"], false, "no lease installed yet");
    assert!(pre["status"].is_null(), "no computed status before install");

    // Install a verified binding via the binary CBOR route.
    let cbor = binding(&key, "serial-CTRL01", now, 100)
        .to_cbor()
        .expect("encode binding");
    let resp = send(
        &h.router,
        post_cbor("/api/v1/licence/lease", ADMIN_TOKEN, cbor),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "a valid binding installs");
    let installed = body_json(resp).await;
    assert_eq!(installed["serial"], "serial-CTRL01");
    assert!(
        installed["valid_to"].as_str().is_some(),
        "the install body carries valid_to"
    );

    // GET now renders the installed tier + the computed ladder state. The store
    // clock is pinned at the grant instant, so the lease is within term →
    // compliant / active. Enforcement is DATA → 200.
    let resp = send(&h.router, support::get("/api/v1/licence", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let post = body_json(resp).await;
    assert_eq!(post["licensed"], true);
    assert_eq!(post["status"]["tier"], "studio");
    assert_eq!(
        post["status"]["state"], "compliant",
        "within term → compliant"
    );
    assert_eq!(
        post["status"]["enforcement"], "active",
        "compliant → active enforcement"
    );
    assert_eq!(
        post["status"]["program_stays_on_air"],
        serde_json::Value::Null,
        "the view does not serialise a program-stays-on-air field"
    );
    assert_eq!(post["status"]["lease"]["serial"], "serial-CTRL01");
    assert_eq!(post["status"]["config_locked"], false);
    assert_eq!(post["status"]["watermark"], false);
}

#[tokio::test]
async fn a_grace_state_is_rendered_when_the_clock_is_past_expiry() {
    // The same installed lease, read at a clock one day past the 35-day term, must
    // render the computed `grace`/`warning` ladder state — proving the GET renders
    // the ladder computed on read at the injected clock, not a stored snapshot.
    let (key, pinned) = keypair();
    let granted = epoch();
    let lease = Lease::new_full(
        "serial-AGE".to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    );
    let in_grace = lease.expires_at + Duration::days(1);
    let h = harness_with(|state| state.with_licence(licence_state(pinned, in_grace)));

    let cbor = binding(&key, "serial-AGE", granted, 100)
        .to_cbor()
        .expect("encode");
    let resp = send(
        &h.router,
        post_cbor("/api/v1/licence/lease", ADMIN_TOKEN, cbor),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(&h.router, support::get("/api/v1/licence", ADMIN_TOKEN)).await;
    let body = body_json(resp).await;
    assert_eq!(body["status"]["state"], "grace");
    assert_eq!(body["status"]["enforcement"], "warning");
    // Even in grace the program stays on air: config is not locked, no watermark.
    assert_eq!(body["status"]["config_locked"], false);
    assert_eq!(body["status"]["watermark"], false);
}

#[tokio::test]
async fn a_tampered_lease_is_rejected_422_signature_invalid() {
    let (key, pinned) = keypair();
    let now = epoch();
    let h = harness_with(|state| state.with_licence(licence_state(pinned, now)));

    // Mutate the covered serial AFTER signing — the signature no longer matches.
    let mut b = binding(&key, "serial-OK", now, 100);
    b.signed.lease.serial = "serial-EVIL".to_owned();
    let cbor = b.to_cbor().expect("encode");

    let resp = send(
        &h.router,
        post_cbor("/api/v1/licence/lease", ADMIN_TOKEN, cbor),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "rejections are RFC-9457 problem documents"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 422);
    assert_eq!(problem["type"], "/problems/signature_invalid");

    // The tampered lease must NOT have installed anything: the resource is still
    // unlicensed (fail-toward-leniency).
    let resp = send(&h.router, support::get("/api/v1/licence", ADMIN_TOKEN)).await;
    assert_eq!(body_json(resp).await["licensed"], false);
}

#[tokio::test]
async fn a_below_threshold_fingerprint_is_rejected_409_fingerprint_mismatch() {
    let (key, pinned) = keypair();
    let now = epoch();
    let h = harness_with(|state| state.with_licence(licence_state(pinned, now)));

    // Score 69 is one below the 70 threshold → a different machine.
    let cbor = binding(&key, "serial-DRIFT", now, 69)
        .to_cbor()
        .expect("encode");
    let resp = send(
        &h.router,
        post_cbor("/api/v1/licence/lease", ADMIN_TOKEN, cbor),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/fingerprint_mismatch");
}

#[tokio::test]
async fn garbage_body_is_rejected_422_never_panics() {
    let (_key, pinned) = keypair();
    let now = epoch();
    let h = harness_with(|state| state.with_licence(licence_state(pinned, now)));

    let resp = send(
        &h.router,
        post_cbor(
            "/api/v1/licence/lease",
            ADMIN_TOKEN,
            vec![0xFF, 0x00, 0x13, 0x37],
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["status"], 422);
}

#[tokio::test]
async fn challenge_export_is_well_formed_cbor() {
    let (_key, pinned) = keypair();
    let now = epoch();
    let h = harness_with(|state| state.with_licence(licence_state(pinned, now)));

    let resp = send(
        &h.router,
        support::get("/api/v1/licence/challenge", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/cbor"),
        "the challenge is served as application/cbor"
    );
    let bytes = body_bytes(resp).await;
    // A CBOR map starts with a major-type-5 header byte (0xA0..=0xBF).
    assert!(!bytes.is_empty(), "the challenge body is non-empty");
    assert!(
        (0xA0..=0xBF).contains(&bytes[0]),
        "first byte is a CBOR map header, got {:#04x}",
        bytes[0]
    );
    // It decodes back through the licence crate's own type (round-trippable).
    let decoded: multiview_licence::ChallengeFile =
        ciborium::from_reader(bytes.as_slice()).expect("challenge decodes as CBOR");
    // Data minimisation: an un-gathered challenge carries no digests.
    assert!(decoded.fingerprint_digests.is_empty());
    assert!(decoded.host_digest.is_empty());
}

#[test]
fn licence_status_doc_mirrors_the_real_view_byte_for_byte() {
    // The licence route serialises the real `multiview_licence::LicenceStatusView`
    // but advertises `openapi_schemas::LicenceStatusDoc` in the OpenAPI document
    // (the licence crate carries no utoipa dep). If the two serde shapes drift,
    // the generated client would be wrong. Pin them: a real view round-trips
    // through the Doc mirror and back without loss.
    let (key, pinned) = keypair();
    let now = epoch();
    let store = LeaseStore::with_clock(Arc::new(move || now));
    store
        .install_binding(&binding(&key, "serial-DRIFTCHK", now, 100), &pinned, now)
        .expect("install");
    let view = store.status().expect("a status after install");

    let view_json = serde_json::to_value(&view).expect("view serialises");
    // The Doc mirror deserialises the real view's JSON without error (every field
    // present + same tags) — and re-serialises to the identical JSON.
    let doc: multiview_control::openapi_schemas::LicenceStatusDoc =
        serde_json::from_value(view_json.clone()).expect("Doc accepts the real view shape");
    let doc_json = serde_json::to_value(&doc).expect("Doc serialises");
    assert_eq!(
        view_json, doc_json,
        "LicenceStatusDoc must mirror LicenceStatusView byte-for-byte (no drift)"
    );
}

#[tokio::test]
async fn get_licence_requires_authentication() {
    let (_key, pinned) = keypair();
    let now = epoch();
    let h = harness_with(|state| state.with_licence(licence_state(pinned, now)));

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/licence")
        .body(Body::empty())
        .expect("request");
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn installing_a_lease_writes_an_account_audit_entry_end_to_end() {
    use multiview_control::{AccountAuditKind, AccountAuditStore, InMemoryAccountAudit};

    let (key, pinned) = keypair();
    let now = epoch();
    // Share an account-audit store so the test can read the seam-written entry.
    let audit: Arc<dyn AccountAuditStore> = Arc::new(InMemoryAccountAudit::new());
    let shared = Arc::clone(&audit);
    let h = harness_with(move |state| {
        state
            .with_licence(licence_state(pinned, now))
            .with_account_audit(Arc::clone(&shared))
    });

    // No account audit entries before the install.
    assert!(audit.page(None, None, 100).entries.is_empty());

    let cbor = binding(&key, "serial-AUDIT01", now, 100)
        .to_cbor()
        .expect("encode binding");
    let resp = send(
        &h.router,
        post_cbor("/api/v1/licence/lease", ADMIN_TOKEN, cbor),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "a valid binding installs");

    // The lease install wrote exactly one append-only audit entry, attributed to
    // the installing principal, carrying the serial (never a raw identifier).
    let page = audit.page(None, Some(AccountAuditKind::LeaseInstall), 100);
    assert_eq!(page.entries.len(), 1, "the install wrote one audit entry");
    let entry = &page.entries[0];
    assert_eq!(entry.kind, AccountAuditKind::LeaseInstall);
    assert_eq!(entry.actor, "admin-key");
    assert_eq!(entry.detail.as_ref().unwrap()["serial"], "serial-AUDIT01");
}
