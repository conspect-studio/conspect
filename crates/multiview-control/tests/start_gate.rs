//! The Conspect **startup gate at the control-plane start command** (Hook 1, S1
//! API surface; ADR-0050 §5).
//!
//! `POST /api/v1/commands/start` starts (a new run of) program output. When the
//! ladder is at the `block-new-instance` rung, that start is refused with the
//! spec's exact reason — surfaced via the start path's existing error route (an
//! RFC-9457 problem). RUNNING program output is never touched: `stop` and the
//! other operational commands stay reachable, and the refusal is a *new-start*
//! convenience block, not an interruption.
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

/// A licence state whose lease is past the 90-day hard bound at the store clock →
/// `block-new-instance`.
fn blocked_state() -> LicenceState {
    let (key, pinned) = keypair();
    let granted = epoch();
    let lease = Lease::new_full(
        "serial-BLK".to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    );
    // 200 days past expiry → well past LEASE_HARD (90d from grant) → lapsed-hard,
    // and the cli's S1 publishes block-new-instance for this lease age. The
    // control store renders the resource state; the start command consults the
    // SAME computed status, so a lapsed-hard lease blocks a new start here.
    let now = lease.expires_at + Duration::days(200);
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    store
        .install_binding(&binding(&key, "serial-BLK", granted), &pinned, now)
        .expect("install");
    LicenceState::new(store, Some(pinned))
}

fn post(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

#[tokio::test]
async fn start_is_refused_when_new_instances_are_blocked() {
    let h = harness_with(|s| s.with_licence(blocked_state()));
    let resp = send(&h.router, post("/api/v1/commands/start", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "starting a NEW run is refused at the block-new-instance rung"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/lease_expired");
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("running ones untouched"),
        "the refusal names the never-off-air promise: {detail}"
    );
}

#[tokio::test]
async fn stop_is_never_refused_running_output_is_untouched() {
    // THE NEVER-OFF-AIR PROOF at the API: even when new starts are blocked, STOP
    // (and every operational command) stays reachable — the block governs only a
    // NEW start, never a running program.
    let h = harness_with(|s| s.with_licence(blocked_state()));
    let resp = send(&h.router, post("/api/v1/commands/stop", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "stop is never refused — running output is untouched"
    );
}

#[tokio::test]
async fn start_succeeds_for_a_compliant_machine() {
    let (key, pinned) = keypair();
    let now = epoch();
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    store
        .install_binding(&binding(&key, "serial-OK", now), &pinned, now)
        .expect("install");
    let h = harness_with(|s| s.with_licence(LicenceState::new(store, Some(pinned))));
    let resp = send(&h.router, post("/api/v1/commands/start", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "a compliant machine starts normally"
    );
}

#[tokio::test]
async fn start_succeeds_for_an_unlicensed_machine() {
    // No installed lease (the default): a start is allowed — the block fires only
    // on positive evidence of a lapsed lease (fail-toward-leniency, ADR-0050 §6.3).
    let h = harness_with(|s| s);
    let resp = send(&h.router, post("/api/v1/commands/start", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}
