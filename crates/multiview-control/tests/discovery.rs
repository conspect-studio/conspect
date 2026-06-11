//! End-to-end tests for the mDNS device-discovery surface (DEV-A5, ADR-M008 §6 /
//! ADR-0041 untrusted-inventory doctrine): the `/discovery/devices` endpoints,
//! the untrusted-inventory snapshot, the `device.discovered` event emission, and
//! the **confirm-adopt boundary** — discovery NEVER auto-creates a device; only
//! the existing `POST /devices/{id}` adopt does. The browse is driven through an
//! **injected `DiscoveryBrowser`** so the whole surface is exercised socket-free,
//! exactly as the NMOS/router socket seams are. Driven through the real router
//! via `tower::oneshot`. Mirrors `tests/devices.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

mod support;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use multiview_control::devices::discovery::{
    infer_driver_kind, DiscoveredService, DiscoveryBrowser, DiscoveryDriverKind,
    DiscoveryInventory, RawDiscoveredService, StaticBrowser, CAST_SERVICE_TYPE, NDI_SERVICE_TYPE,
};
use multiview_events::{AddressFamily, Event};
use serde_json::json;
use support::{body_json, get, harness_with, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

/// A raw zowietek/NDI service the injected browser will yield: both an IPv6
/// (AAAA) and an IPv4 (legacy) address, plus a TXT record.
fn raw_ndi() -> RawDiscoveredService {
    RawDiscoveredService::new(
        NDI_SERVICE_TYPE,
        "ZowieBox-Foyer",
        "zowiebox-foyer.local.",
        5961,
        vec![
            "192.0.2.42".parse::<IpAddr>().unwrap(),
            "fd00:db8::42".parse::<IpAddr>().unwrap(),
        ],
        vec![("model".to_owned(), "ZowieBox".to_owned())],
    )
}

/// A raw Cast service that advertises a non-8009 port (Cast groups do).
fn raw_cast() -> RawDiscoveredService {
    RawDiscoveredService::new(
        CAST_SERVICE_TYPE,
        "Foyer-Display",
        "chromecast-foyer.local.",
        42001,
        vec!["fd00:db8::99".parse::<IpAddr>().unwrap()],
        vec![("fn".to_owned(), "Foyer Display".to_owned())],
    )
}

// ---- Pure model: driver-kind inference ----

#[test]
fn driver_kind_inferred_from_service_type() {
    assert_eq!(
        infer_driver_kind(CAST_SERVICE_TYPE, None),
        DiscoveryDriverKind::Cast
    );
    assert_eq!(
        infer_driver_kind(NDI_SERVICE_TYPE, None),
        DiscoveryDriverKind::NdiSource
    );
    // An unknown service type is honestly Unknown, never guessed.
    assert_eq!(
        infer_driver_kind("_http._tcp.local.", None),
        DiscoveryDriverKind::Unknown
    );
}

#[test]
fn zowietek_control_service_type_is_only_recognised_when_configured() {
    // The vendor's control-API mDNS service type is UNVERIFIED (best-effort): it
    // is only recognised when the operator configures the browse string, never
    // fabricated from a guessed constant.
    let configured = "_zowietek._tcp.local.";
    assert_eq!(
        infer_driver_kind(configured, Some(configured)),
        DiscoveryDriverKind::ZowietekControl
    );
    // Without the configured string, the same type is Unknown — we never claim a
    // service type we have not verified.
    assert_eq!(
        infer_driver_kind(configured, None),
        DiscoveryDriverKind::Unknown
    );
}

// ---- Pure model: AAAA-first ordering, IPv4 legacy labelling ----

#[test]
fn endpoints_are_aaaa_first_with_ipv4_labelled_legacy() {
    let svc = DiscoveredService::from_raw(&raw_ndi(), None, far_future());
    // IPv6 leads even though IPv4 was listed first in the raw record.
    assert_eq!(svc.endpoints[0].family, AddressFamily::Ipv6);
    assert!(svc.endpoints[0].address.contains("fd00:db8::42"));
    assert_eq!(svc.endpoints[1].family, AddressFamily::Ipv4Legacy);
    // The IPv6 management address brackets the literal (URL-safe).
    assert!(svc.endpoints[0].address.starts_with('['));
}

#[test]
fn primary_address_prefers_ipv6() {
    let svc = DiscoveredService::from_raw(&raw_ndi(), None, far_future());
    assert_eq!(svc.primary().family, AddressFamily::Ipv6);
}

// ---- Pure model: dedup + TTL expiry ----

#[test]
fn inventory_dedups_by_key_latest_wins() {
    let inv = DiscoveryInventory::new(8);
    inv.upsert(DiscoveredService::from_raw(&raw_ndi(), None, far_future()));
    // A second sighting of the SAME service (same key) replaces, not duplicates.
    inv.upsert(DiscoveredService::from_raw(&raw_ndi(), None, far_future()));
    assert_eq!(inv.snapshot().len(), 1);
}

#[test]
fn inventory_expires_stale_rows_on_snapshot() {
    let inv = DiscoveryInventory::new(8);
    // A row that already expired (deadline in the past) is purged on read.
    inv.upsert(DiscoveredService::from_raw(
        &raw_ndi(),
        None,
        already_expired(),
    ));
    assert!(inv.snapshot().is_empty());
}

#[test]
fn inventory_is_bounded_drop_oldest() {
    let inv = DiscoveryInventory::new(2);
    for n in 0..5 {
        let mut raw = raw_cast();
        raw.instance_name = format!("Cast-{n}");
        inv.upsert(DiscoveredService::from_raw(&raw, None, far_future()));
    }
    // Never grows past the cap (bounded, invariant #10).
    assert!(inv.snapshot().len() <= 2);
}

// ---- Endpoints: GET returns the untrusted inventory ----

#[tokio::test]
async fn get_discovery_is_initially_empty() {
    let h = harness_with(|s| s);
    let resp = send(&h.router, get("/api/v1/discovery/devices", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body.as_array().expect("an array").len(), 0);
}

#[tokio::test]
async fn get_discovery_requires_read_role() {
    let h = harness_with(|s| s);
    // No token → 401.
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/discovery/devices")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---- Endpoints: scan kicks a browse, returns 202, streams device.discovered ----

#[tokio::test]
async fn scan_returns_202_and_populates_untrusted_inventory() {
    let browser: Arc<dyn DiscoveryBrowser> =
        Arc::new(StaticBrowser::new(vec![raw_ndi(), raw_cast()]));
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));

    // A viewer cannot scan (write role required).
    let resp = send(
        &h.router,
        post_json("/api/v1/discovery/devices/scan", VIEWER_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // An operator kicks the scan: 202 + an operation id.
    let resp = send(
        &h.router,
        post_json("/api/v1/discovery/devices/scan", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let accepted = body_json(resp).await;
    assert!(accepted["operation_id"].as_str().is_some());

    // The browse task runs to completion off the request path; poll the
    // inventory until both services land (bounded wait).
    let mut found = 0;
    for _ in 0..50 {
        let resp = send(&h.router, get("/api/v1/discovery/devices", OPERATOR_TOKEN)).await;
        let body = body_json(resp).await;
        found = body.as_array().map_or(0, Vec::len);
        if found >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(found, 2, "both discovered services land in the inventory");
}

#[tokio::test]
async fn scan_emits_device_discovered_events_aaaa_first() {
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(StaticBrowser::new(vec![raw_ndi()]));
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));
    let mut sub = h.engine.subscribe();

    let resp = send(
        &h.router,
        post_json("/api/v1/discovery/devices/scan", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The discovery row is published as a device.discovered event, IPv6-first.
    let mut discovered = None;
    for _ in 0..100 {
        if let Ok(env) = sub.try_recv() {
            if let Event::DeviceDiscovered(d) = &*env.event {
                discovered = Some(d.clone());
                break;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let d = discovered.expect("a device.discovered event was published");
    assert_eq!(d.driver, "ndi-source");
    assert_eq!(d.family, AddressFamily::Ipv6);
    assert!(d.address.contains("fd00:db8::42"));
}

// ---- The confirm-adopt boundary: discovery NEVER auto-creates a device ----

#[tokio::test]
async fn scan_never_auto_creates_a_device() {
    let browser: Arc<dyn DiscoveryBrowser> =
        Arc::new(StaticBrowser::new(vec![raw_ndi(), raw_cast()]));
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));

    send(
        &h.router,
        post_json("/api/v1/discovery/devices/scan", OPERATOR_TOKEN, &json!({})),
    )
    .await;

    // Wait for the discovery inventory to fill.
    for _ in 0..50 {
        let resp = send(&h.router, get("/api/v1/discovery/devices", OPERATOR_TOKEN)).await;
        if body_json(resp).await.as_array().map_or(0, Vec::len) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The managed-device registry is STILL EMPTY — discovery informs, it never
    // adopts (ADR-0041 doctrine).
    let resp = send(&h.router, get("/api/v1/devices", OPERATOR_TOKEN)).await;
    let devices = body_json(resp).await;
    assert_eq!(
        devices.as_array().expect("an array").len(),
        0,
        "discovery must NEVER create a registry device — only an explicit adopt does"
    );
}

#[tokio::test]
async fn confirm_adopt_is_the_separate_devices_post_referencing_a_discovered_address() {
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(StaticBrowser::new(vec![raw_ndi()]));
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));

    send(
        &h.router,
        post_json("/api/v1/discovery/devices/scan", OPERATOR_TOKEN, &json!({})),
    )
    .await;

    // Read the untrusted inventory and take the discovered IPv6 address.
    let mut primary = None;
    for _ in 0..50 {
        let resp = send(&h.router, get("/api/v1/discovery/devices", OPERATOR_TOKEN)).await;
        let body = body_json(resp).await;
        if let Some(row) = body.as_array().and_then(|a| a.first()).cloned() {
            primary = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let row = primary.expect("a discovered row");
    let address = row["primary_address"].as_str().expect("a primary address");
    assert!(address.contains("fd00:db8::42"));

    // The operator confirms-adopts by POSTing to the EXISTING /devices/{id}
    // endpoint referencing the discovered address (discovery itself created
    // nothing).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/dev-foyer",
            OPERATOR_TOKEN,
            &json!({
                "name": "Foyer ZowieBox",
                "body": {
                    "id": "dev-foyer",
                    "driver": "zowietek",
                    "address": address,
                    "desired_mode": "decoder"
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Now — and only now — the device exists in the registry.
    let resp = send(&h.router, get("/api/v1/devices", OPERATOR_TOKEN)).await;
    assert_eq!(body_json(resp).await.as_array().expect("an array").len(), 1);
}

/// A TTL deadline far in the future (rows stay live for the test).
fn far_future() -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(3600)
}

/// A TTL deadline already in the past (the row is stale immediately).
fn already_expired() -> std::time::Instant {
    std::time::Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(std::time::Instant::now)
}
