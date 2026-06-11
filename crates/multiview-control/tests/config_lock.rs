//! The Conspect **config-lock interceptor** (Hook 2, S2 backend; ADR-0050 §5/§6).
//!
//! When the entitlement ladder is at a `config_locked()` rung (lapsed-hard, the
//! `config-locked`/`watermark`/`block-new-instance` levels), control-plane
//! *configuration* mutations (resource create/update/delete + config apply)
//! return an RFC-9457 problem that names the ladder reason and links
//! `/settings/licence`. READ endpoints and operational continuity (start/stop,
//! the lease install that *recovers* the lock) are unaffected.
//!
//! THE SACRED CONSTRAINT (invariant #1/#10): this interceptor reads a store and
//! returns a problem document — it holds no engine handle and can neither stop a
//! running program nor back-pressure the engine. A locked config is a denied
//! *reconfiguration*; the running scene keeps playing.
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
use support::{body_json, harness_with, send, ADMIN_TOKEN};

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = OsRng;
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

fn binding(key: &SigningKey, serial: &str, granted: DateTime<Utc>) -> LeaseBinding {
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
        100,
    )
}

/// A licence state whose installed lease is **lapsed-hard** (config-locked) at
/// the store's pinned clock: granted at epoch, read 60 days past expiry (> the
/// 45-day soft-lapse boundary).
fn config_locked_state() -> LicenceState {
    let (key, pinned) = keypair();
    let granted = epoch();
    // 60 days past expiry → lapsed-hard → config_locked + watermark.
    let lease = Lease::new_full(
        "serial-LOCK".to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    );
    let now = lease.expires_at + Duration::days(60);
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    store
        .install_binding(&binding(&key, "serial-LOCK", granted), &pinned, now)
        .expect("install lapsed-hard lease");
    // Sanity: the computed status really is config-locked.
    let status = store.status().expect("status");
    assert!(
        status.config_locked,
        "the fixture must be config-locked, got state {:?}",
        status.state
    );
    LicenceState::new(store, Some(pinned))
}

/// A compliant licence state (within term) — never locked.
fn compliant_state() -> LicenceState {
    let (key, pinned) = keypair();
    let now = epoch();
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    store
        .install_binding(&binding(&key, "serial-OK", now), &pinned, now)
        .expect("install compliant lease");
    LicenceState::new(store, Some(pinned))
}

fn put_layout(id: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/v1/layouts/{id}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "name": id, "body": {} }).to_string(),
        ))
        .expect("request")
}

#[tokio::test]
async fn a_config_mutation_is_locked_with_a_ladder_problem() {
    let h = harness_with(|s| s.with_licence(config_locked_state()));

    // A resource CREATE (config mutation) is refused with an RFC-9457 problem
    // naming the ladder reason + linking /settings/licence.
    let resp = send(&h.router, put_layout("new-layout", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a config mutation under config-lock is refused"
    );
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "the refusal is an RFC-9457 problem"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/config_locked");
    assert_eq!(problem["status"], 409);
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("/settings/licence"),
        "the problem links the licence settings: {detail}"
    );
    assert!(
        detail.to_lowercase().contains("lease"),
        "the problem names the ladder reason: {detail}"
    );
}

#[tokio::test]
async fn reads_are_unaffected_by_config_lock() {
    let h = harness_with(|s| s.with_licence(config_locked_state()));

    // A GET still answers 200 — reads + operational continuity are never locked.
    let resp = send(&h.router, support::get("/api/v1/layouts", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reads are never locked (operational continuity)"
    );

    // The licence resource itself is still readable.
    let resp = send(&h.router, support::get("/api/v1/licence", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_lease_install_recovery_path_is_never_locked() {
    // Installing a FRESH lease is how you RECOVER from the lock — it must never be
    // intercepted by the config-lock guard (else the machine could never unlock).
    let h = harness_with(|s| s.with_licence(config_locked_state()));

    // A garbage CBOR body proves the request REACHED the install handler (422),
    // rather than being short-circuited by the config-lock guard (409).
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/licence/lease")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/cbor")
        .body(Body::from(vec![0xFF, 0x00]))
        .expect("request");
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the lease install path is reachable under lock (the recovery path)"
    );
}

#[tokio::test]
async fn operational_start_stop_is_not_config_locked() {
    // Operational continuity: start/stop are NOT configuration mutations and must
    // remain reachable under config-lock (they return 202, the existing contract).
    let h = harness_with(|s| s.with_licence(config_locked_state()));
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/commands/stop")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request");
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "operational commands stay reachable under config-lock"
    );
}

#[tokio::test]
async fn a_compliant_machine_mutates_freely() {
    let h = harness_with(|s| s.with_licence(compliant_state()));
    let resp = send(&h.router, put_layout("free-layout", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "a compliant machine's config mutations are never locked"
    );
}

#[tokio::test]
async fn an_unlicensed_machine_is_not_config_locked() {
    // No installed lease (the default unlicensed state) does NOT lock config —
    // the lock fires only on positive evidence of a lapsed lease (fail-toward-
    // leniency, ADR-0050 §6.3).
    let h = harness_with(|s| s);
    let resp = send(&h.router, put_layout("anon-layout", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "an unlicensed (no-lease) machine is not config-locked"
    );
}
