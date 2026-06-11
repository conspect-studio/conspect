//! The Conspect **heartbeat-status read surface** (Hook 4, ADR-0050 §3 / brief
//! §7, §11 — the two-pipe heartbeat status).
//!
//! `GET /api/v1/licensing/heartbeat-status` is **read-only** — the spec mandates
//! NO mutating endpoint exists for it. With no server client yet (blocked on the
//! external wire-protocol doc, brief §14 O1), it reports **honestly** from local
//! lease state: a file-sourced lease reports transport `"file"`, `next_due` from
//! the lease's next-contact field, `last_at` = the lease install instant, and the
//! spec's exhaustive `payload_fields` list.
//!
//! Read-only data: this surface holds no engine handle and cannot affect output
//! (inv #1/#10).
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
use support::{body_json, harness_with, send, ADMIN_TOKEN, VIEWER_TOKEN};

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = OsRng;
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

/// A signed binding from a `source` lease (online/relay/file) at `granted`.
fn binding(
    key: &SigningKey,
    serial: &str,
    granted: DateTime<Utc>,
    source: LeaseSource,
) -> LeaseBinding {
    let lease = match source {
        LeaseSource::File => Lease::new_offline(serial.to_owned(), granted, ACTIVATION_WINDOW_DAYS),
        other => Lease::new_full(serial.to_owned(), granted, other, ACTIVATION_WINDOW_DAYS),
    };
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

/// A licence state with a `source` lease installed at the fixed `now`.
fn state_with(source: LeaseSource) -> (LicenceState, DateTime<Utc>, Lease) {
    let (key, pinned) = keypair();
    let now = epoch();
    let store = Arc::new(LeaseStore::with_clock(Arc::new(move || now)));
    let b = binding(&key, "serial-HB", now, source);
    let lease = b.entitlement.lease.clone();
    store
        .install_binding(&b, &pinned, now)
        .expect("install lease");
    (LicenceState::new(store, Some(pinned)), now, lease)
}

#[tokio::test]
async fn file_lease_reports_file_transport_and_the_honest_local_shape() {
    let (licence, install_at, lease) = state_with(LeaseSource::File);
    let h = harness_with(|s| s.with_licence(licence));

    let resp = send(
        &h.router,
        support::get("/api/v1/licensing/heartbeat-status", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "read-only surface answers 200"
    );
    let body = body_json(resp).await;

    // A file-sourced lease reports transport "file".
    assert_eq!(body["transport"], "file");
    // last_at = the lease INSTALL instant (honest local-state report).
    assert_eq!(body["last_at"], install_at.to_rfc3339());
    // next_due = the lease's next-contact-due field.
    assert_eq!(body["next_due"], lease.next_contact_due.to_rfc3339());

    // payload_fields = the spec's exhaustive list (licence id, salted fingerprint
    // digest vector, app version, lease serial) — reported, never a mutating knob.
    let fields: Vec<String> = body["payload_fields"]
        .as_array()
        .expect("payload_fields is an array")
        .iter()
        .map(|v| v.as_str().expect("string").to_owned())
        .collect();
    assert!(fields.iter().any(|f| f.contains("licence")), "{fields:?}");
    assert!(
        fields.iter().any(|f| f.contains("fingerprint")),
        "{fields:?}"
    );
    assert!(fields.iter().any(|f| f.contains("version")), "{fields:?}");
    assert!(fields.iter().any(|f| f.contains("serial")), "{fields:?}");
}

#[tokio::test]
async fn online_lease_reports_direct_transport() {
    let (licence, _at, _lease) = state_with(LeaseSource::Online);
    let h = harness_with(|s| s.with_licence(licence));
    let resp = send(
        &h.router,
        support::get("/api/v1/licensing/heartbeat-status", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["transport"], "direct");
}

#[tokio::test]
async fn relay_lease_reports_relay_transport() {
    let (licence, _at, _lease) = state_with(LeaseSource::Relay);
    let h = harness_with(|s| s.with_licence(licence));
    let resp = send(
        &h.router,
        support::get("/api/v1/licensing/heartbeat-status", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["transport"], "relay");
}

#[tokio::test]
async fn no_lease_reports_an_honest_unlicensed_shape() {
    // No installed lease (the default state): the surface answers 200 with a null
    // last_at/next_due (honest "no heartbeat yet"), never a 5xx.
    let h = harness_with(|s| s);
    let resp = send(
        &h.router,
        support::get("/api/v1/licensing/heartbeat-status", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["last_at"].is_null(), "no lease → no last contact");
    assert!(body["next_due"].is_null(), "no lease → no next due");
    // The payload_fields list is still reported (the shape is fixed).
    assert!(body["payload_fields"].as_array().is_some());
}

#[tokio::test]
async fn heartbeat_status_requires_authentication() {
    let (licence, _at, _lease) = state_with(LeaseSource::File);
    let h = harness_with(|s| s.with_licence(licence));
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/licensing/heartbeat-status")
        .body(axum::body::Body::empty())
        .expect("request");
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn heartbeat_status_has_no_mutating_verb() {
    // The spec mandates NO mutating endpoint exists. A POST to the path must NOT
    // be routed to a handler (405 Method Not Allowed, not 200/202).
    let (licence, _at, _lease) = state_with(LeaseSource::File);
    let h = harness_with(|s| s.with_licence(licence));
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/licensing/heartbeat-status")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {ADMIN_TOKEN}"),
        )
        .body(axum::body::Body::empty())
        .expect("request");
    let resp = send(&h.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "heartbeat-status is read-only; no mutating verb exists"
    );
}
