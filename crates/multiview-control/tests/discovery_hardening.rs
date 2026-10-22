//! Adversarial-review hardening tests for the mDNS discovery surface (DEV-A5
//! review findings 1–6):
//!
//! * **F1** — the scan drain is *interleaved* across all browse receivers: a
//!   chatty first receiver (mdns-sd keeps `_googlecast._tcp` alive with
//!   periodic `SearchStarted` keepalives) must never starve the later
//!   receivers, and events queued at the deadline are swept, not dropped.
//!   The daemon-side proof: every receiver is drained continuously, so the
//!   daemon's bounded(10) blocking send can never wedge on our channel.
//! * **F6** — hostile-responder bounds: the transient event vec is capped and
//!   oversized advertised names/hosts/TXT records are truncated.
//! * **F3** — an `Idempotency-Key` replay returns the original 202/op id
//!   WITHOUT re-executing the browse.
//! * **F2** — scans are single-flight: a concurrent scan request attaches to
//!   the running scan's operation id instead of spawning a second
//!   (mutually-destructive) browse; an attached key replays the running op.
//! * **F4** — `device.discovered` events carry the scan's operation id as the
//!   envelope `corr` (ADR-RT007), window-fenced so an earlier scan's
//!   stragglers are never stamped with a newer scan's id.
//! * **F5** — the operator-configured zowietek-control service type is a real
//!   config knob (`[discovery]`), threaded config → scan → classification.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::net::{IpAddr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::DiscoveryConfig;
use multiview_control::devices::discovery::{
    drain_interleaved, scan_service_types, DiscoveredService, DiscoveryBrowser, DrainReceiver,
    RawDiscoveredService, StaticBrowser, CAST_SERVICE_TYPE, MAX_ENDPOINTS, MAX_FIELD_LEN,
    MAX_TXT_RECORDS, MAX_TXT_VALUE_LEN, NDI_SERVICE_TYPE,
};
use multiview_control::devices::DeviceBroadcaster;
use multiview_control::realtime::{CorrKey, CorrRegistry};
use multiview_control::{DeviceStatusRegistry, EngineStateSnapshot, OperationId, SessionStream};
use multiview_engine::EnginePublisher;
use multiview_events::{AddressFamily, Event};
use serde_json::json;
use support::{body_json, get, harness_with, send, OPERATOR_TOKEN};

// ---- Shared fixtures ----

/// A raw NDI service the injected browser yields.
fn raw_ndi() -> RawDiscoveredService {
    RawDiscoveredService::new(
        NDI_SERVICE_TYPE,
        "ZowieBox-Foyer",
        "zowiebox-foyer.local.",
        5961,
        vec!["fd00:db8::42".parse::<IpAddr>().unwrap()],
        vec![("model".to_owned(), "ZowieBox".to_owned())],
    )
}

/// Build a scan `POST` with an `Idempotency-Key` header.
fn post_scan(token: &str, idempotency_key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/discovery/devices/scan")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    builder
        .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
        .expect("request should build")
}

/// POST a scan and return the `202` body's operation id.
async fn scan_op(router: &axum::Router, key: Option<&str>) -> String {
    let resp = send(router, post_scan(OPERATOR_TOKEN, key)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    body_json(resp).await["operation_id"]
        .as_str()
        .expect("an operation id")
        .to_owned()
}

/// Poll the inventory until it holds at least `want` rows (bounded wait).
async fn wait_for_inventory(router: &axum::Router, want: usize) -> Vec<serde_json::Value> {
    for _ in 0..100 {
        let resp = send(router, get("/api/v1/discovery/devices", OPERATOR_TOKEN)).await;
        let body = body_json(resp).await;
        let rows = body.as_array().cloned().unwrap_or_default();
        if rows.len() >= want {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("inventory never reached {want} rows");
}

// ---- F1: the interleaved drain ----

/// A [`DrainReceiver`] over a plain tokio mpsc channel (the CI-testable stand-in
/// for an `mdns-sd` browse channel).
struct ChanReceiver(tokio::sync::mpsc::Receiver<u32>);

impl DrainReceiver for ChanReceiver {
    type Event = u32;

    fn recv(&mut self) -> impl std::future::Future<Output = Option<u32>> + Send {
        self.0.recv()
    }

    fn try_recv(&mut self) -> Option<u32> {
        self.0.try_recv().ok()
    }
}

#[tokio::test]
async fn drain_interleaved_collects_late_receivers_despite_a_chatty_first_receiver() {
    // The chatty receiver mimics `_googlecast._tcp` + the mdns-sd keepalives: a
    // bounded(4) channel whose producer keeps it full for the whole budget. The
    // old sequential drain burned the ENTIRE budget here and dropped the later
    // receivers' queued events.
    let (chatty_tx, chatty_rx) = tokio::sync::mpsc::channel::<u32>(4);
    tokio::spawn(async move {
        for n in 0..u32::MAX {
            if chatty_tx.send(n).await.is_err() {
                break;
            }
            if n % 16 == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    // Two late receivers (the NDI / zowietek browses) deliver mid-budget.
    let (late_a_tx, late_a_rx) = tokio::sync::mpsc::channel::<u32>(4);
    let (late_b_tx, late_b_rx) = tokio::sync::mpsc::channel::<u32>(4);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = late_a_tx.send(1_000_001).await;
        let _ = late_a_tx.send(1_000_002).await;
        // Keep the sender alive past the deadline so the channel never closes.
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = late_b_tx.send(2_000_001).await;
        let _ = late_b_tx.send(2_000_002).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let out = drain_interleaved(
        vec![
            ChanReceiver(chatty_rx),
            ChanReceiver(late_a_rx),
            ChanReceiver(late_b_rx),
        ],
        Duration::from_millis(300),
        100_000,
    )
    .await;

    for marker in [1_000_001, 1_000_002, 2_000_001, 2_000_002] {
        assert!(
            out.contains(&marker),
            "late receiver event {marker} must be collected even when the first \
             receiver is chatty (got {} events)",
            out.len()
        );
    }
    // The chatty channel was drained continuously — its bounded(4) buffer never
    // wedged its producer for the scan duration (the daemon-can't-block proof).
    let chatty_count = out.iter().filter(|v| **v < 1_000_000).count();
    assert!(
        chatty_count > 100,
        "the chatty receiver must be drained throughout the budget, got {chatty_count}"
    );
}

#[tokio::test]
async fn drain_interleaved_sweeps_queued_events_at_the_deadline() {
    let (tx, rx) = tokio::sync::mpsc::channel::<u32>(8);
    for n in [7, 8, 9] {
        tx.send(n).await.unwrap();
    }
    // Zero budget: the deadline has already passed when the drain starts; the
    // final non-blocking sweep must still collect everything already queued
    // instead of dropping it on the floor.
    let out = drain_interleaved(vec![ChanReceiver(rx)], Duration::ZERO, 100).await;
    assert_eq!(out, vec![7, 8, 9]);
    drop(tx);
}

// ---- F6: hostile-responder bounds ----

#[tokio::test]
async fn drain_interleaved_caps_total_collected_events() {
    let (tx_a, rx_a) = tokio::sync::mpsc::channel::<u32>(64);
    let (tx_b, rx_b) = tokio::sync::mpsc::channel::<u32>(64);
    for n in 0..50 {
        tx_a.send(n).await.unwrap();
        tx_b.send(1_000 + n).await.unwrap();
    }
    let out = drain_interleaved(
        vec![ChanReceiver(rx_a), ChanReceiver(rx_b)],
        Duration::ZERO,
        8,
    )
    .await;
    assert_eq!(out.len(), 8, "the transient event vec is capped");
    // The cap is split fairly: a hostile flood on one receiver cannot spend
    // another receiver's share.
    let a = out.iter().filter(|v| **v < 1_000).count();
    let b = out.len() - a;
    assert_eq!((a, b), (4, 4), "the cap is shared across receivers");
}

#[test]
fn from_raw_truncates_hostile_oversized_fields() {
    let big = "x".repeat(10_000);
    let addresses: Vec<IpAddr> = (0..100u16)
        .map(|n| IpAddr::V6(Ipv6Addr::new(0xfd00, 0xdb8, 0, 0, 0, 0, 0, n)))
        .collect();
    let txt: Vec<(String, String)> = (0..500)
        .map(|n| (format!("k{n}-{big}"), big.clone()))
        .collect();
    let raw = RawDiscoveredService::new(
        format!("_evil{big}._tcp.local."),
        big.clone(),
        big.clone(),
        8080,
        addresses,
        txt,
    );
    let svc = DiscoveredService::from_raw(
        &raw,
        None,
        std::time::Instant::now() + Duration::from_secs(60),
        None,
    );
    assert!(svc.name.len() <= MAX_FIELD_LEN, "name is truncated");
    assert!(svc.host.len() <= MAX_FIELD_LEN, "host is truncated");
    assert!(
        svc.service_type.len() <= MAX_FIELD_LEN,
        "service type is truncated"
    );
    assert!(
        svc.txt.len() <= MAX_TXT_RECORDS,
        "TXT record count is capped"
    );
    for record in &svc.txt {
        assert!(record.key.len() <= MAX_FIELD_LEN, "TXT key is truncated");
        assert!(
            record.value.len() <= MAX_TXT_VALUE_LEN,
            "TXT value is truncated"
        );
    }
    assert!(
        svc.endpoints.len() <= MAX_ENDPOINTS,
        "endpoint count is capped"
    );
}

// ---- F3: Idempotency-Key replay never re-executes ----

/// A browser that counts its `browse` invocations (the browser seam call count
/// the replay/single-flight contracts are asserted against).
struct CountingBrowser {
    inner: StaticBrowser,
    calls: Arc<AtomicUsize>,
}

impl DiscoveryBrowser for CountingBrowser {
    fn browse(&self, service_types: &[String], budget: Duration) -> Vec<RawDiscoveredService> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.browse(service_types, budget)
    }
}

#[tokio::test]
async fn scan_replay_returns_original_op_without_rebrowsing() {
    let calls = Arc::new(AtomicUsize::new(0));
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(CountingBrowser {
        inner: StaticBrowser::new(vec![raw_ndi()]),
        calls: Arc::clone(&calls),
    });
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));

    let op1 = scan_op(&h.router, Some("replay-key")).await;
    // Let the first scan run to completion.
    wait_for_inventory(&h.router, 1).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The replayed Idempotency-Key returns the ORIGINAL op id WITHOUT
    // re-executing the browse (the canonical routes/mod.rs replay semantics).
    let op2 = scan_op(&h.router, Some("replay-key")).await;
    assert_eq!(op2, op1, "a replay answers with the original operation id");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a replayed Idempotency-Key must not re-run the browse"
    );
}

// ---- F2: single-flight scans ----

/// A browser whose `browse` blocks (on a blocking thread) until released, so a
/// test can hold a scan "running" deterministically. A 5 s safety valve keeps a
/// buggy run from hanging the suite.
struct GatedBrowser {
    release: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl DiscoveryBrowser for GatedBrowser {
    fn browse(&self, _service_types: &[String], _budget: Duration) -> Vec<RawDiscoveredService> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let start = std::time::Instant::now();
        while !self.release.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        vec![raw_ndi()]
    }
}

#[tokio::test]
async fn concurrent_scans_single_flight_attach_to_the_running_scan() {
    let release = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(GatedBrowser {
        release: Arc::clone(&release),
        calls: Arc::clone(&calls),
    });
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));

    // First POST claims the single-flight slot (synchronously, before 202).
    let op1 = scan_op(&h.router, None).await;
    // A concurrent POST attaches to the RUNNING scan instead of spawning a
    // second browse (mdns-sd listeners/queriers are mutually destructive).
    let op2 = scan_op(&h.router, None).await;
    assert_eq!(
        op2, op1,
        "a concurrent scan attaches to the running scan's op"
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        calls.load(Ordering::SeqCst) <= 1,
        "exactly one browse may run at a time"
    );

    // After the running scan completes, a fresh POST starts a NEW scan.
    release.store(true, Ordering::SeqCst);
    let mut op3 = op1.clone();
    for _ in 0..100 {
        op3 = scan_op(&h.router, None).await;
        if op3 != op1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(op3, op1, "a scan after completion mints a fresh operation");
    for _ in 0..100 {
        if calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the new scan browses again"
    );
}

#[tokio::test]
async fn attached_idempotency_key_replays_the_running_scan_op() {
    let release = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(GatedBrowser {
        release: Arc::clone(&release),
        calls: Arc::clone(&calls),
    });
    let h = harness_with(move |s| s.with_discovery_browser(Arc::clone(&browser)));

    let op1 = scan_op(&h.router, None).await;
    // A keyed POST while the scan runs: attaches, and the key is re-pointed at
    // the RUNNING op (the operation that actually executed).
    let op2 = scan_op(&h.router, Some("att-key")).await;
    assert_eq!(op2, op1, "the keyed request attaches to the running scan");
    // Replaying the key answers with the running scan's op, not a phantom id
    // for an operation that never executed.
    let op3 = scan_op(&h.router, Some("att-key")).await;
    assert_eq!(op3, op1, "the replayed key echoes the running scan's op");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "still exactly one browse");
    release.store(true, Ordering::SeqCst);
}

// ---- F4: device.discovered carries the scan op as corr ----

#[tokio::test]
async fn discovered_events_carry_the_scan_operation_id_as_corr() {
    let corr = Arc::new(CorrRegistry::new(64));
    let corr_for_state = Arc::clone(&corr);
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(StaticBrowser::new(vec![raw_ndi()]));
    let h = harness_with(move |mut s| {
        s.corr = corr_for_state;
        s.with_discovery_browser(Arc::clone(&browser))
    });
    let mut session = SessionStream::new(h.engine.subscribe(), "sess-disc-corr", None)
        .with_corr_registry(Arc::clone(&corr));

    let op = scan_op(&h.router, None).await;

    let delta = tokio::time::timeout(Duration::from_secs(2), session.next_delta())
        .await
        .expect("a discovered event arrives within 2s")
        .unwrap()
        .expect("a delta is delivered");
    assert!(
        matches!(&delta.envelope.payload, Event::DeviceDiscovered(_)),
        "expected device.discovered, got {:?}",
        delta.envelope.payload
    );
    assert_eq!(
        delta.envelope.corr.as_deref(),
        Some(op.as_str()),
        "device.discovered must echo the scan's operation id as corr (ADR-RT007)"
    );
}

#[tokio::test]
async fn corr_window_does_not_stamp_pre_window_events() {
    let engine: Arc<EnginePublisher<EngineStateSnapshot, Event>> =
        Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(CorrRegistry::new(8));
    let broadcaster =
        DeviceBroadcaster::new(Arc::clone(&engine), Arc::new(DeviceStatusRegistry::new()));
    let mut session = SessionStream::new(engine.subscribe(), "sess-window", None)
        .with_corr_registry(Arc::clone(&registry));

    // An event published BEFORE the window opens (an earlier scan's straggler).
    broadcaster.discovered(
        "ndi-source",
        "[fd00:db8::1]:5961",
        AddressFamily::Ipv6,
        None,
        None,
    );
    let fence = engine.events.sequence();
    let op = OperationId::new();
    registry.record_window(CorrKey::Discovery, op.clone(), fence);
    // An event published INSIDE the window.
    broadcaster.discovered(
        "ndi-source",
        "[fd00:db8::2]:5961",
        AddressFamily::Ipv6,
        None,
        None,
    );

    let first = session
        .next_delta()
        .await
        .unwrap()
        .expect("the pre-window event is delivered");
    assert_eq!(
        first.envelope.corr, None,
        "a pre-window event must stay uncorrelated — never stamped with a \
         newer scan's id"
    );
    let second = session
        .next_delta()
        .await
        .unwrap()
        .expect("the in-window event is delivered");
    assert_eq!(
        second.envelope.corr.as_deref(),
        Some(op.as_str()),
        "an in-window event echoes the window's op id"
    );
}

// ---- F5: the configured zowietek-control service type is a real knob ----

#[tokio::test]
async fn configured_zowietek_service_type_is_browsed_and_classified() {
    let cfg_ty = "_zowietek-ctl._tcp.local.";
    let raw = RawDiscoveredService::new(
        cfg_ty,
        "ZB-Control",
        "zb-ctl.local.",
        80,
        vec!["fd00:db8::77".parse::<IpAddr>().unwrap()],
        vec![],
    );
    let browser: Arc<dyn DiscoveryBrowser> = Arc::new(StaticBrowser::new(vec![raw]));
    let config = DiscoveryConfig::new(Some(cfg_ty.to_owned()), Vec::new());
    let h = harness_with(move |s| {
        s.with_discovery_browser(Arc::clone(&browser))
            .with_discovery_config(config)
    });

    // The 202 body lists the configured type among the browsed service types.
    let resp = send(&h.router, post_scan(OPERATOR_TOKEN, None)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let types: Vec<String> = body["service_types"]
        .as_array()
        .expect("service_types is an array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    assert!(
        types.iter().any(|t| t == cfg_ty),
        "the configured zowietek type is browsed: {types:?}"
    );

    // The discovered service classifies as zowietek-control (it only appears
    // at all because the configured type was actually requested — the static
    // browser filters by requested type).
    let rows = wait_for_inventory(&h.router, 1).await;
    assert_eq!(rows[0]["driver_kind"], "zowietek-control");
}

#[test]
fn scan_service_types_include_configured_and_extra_types_deduplicated() {
    let types = scan_service_types(
        Some("_zow._tcp"),
        &["_extra._udp.local.".to_owned(), "_ndi._tcp".to_owned()],
    );
    assert!(types.iter().any(|t| t == CAST_SERVICE_TYPE));
    assert!(types.iter().any(|t| t == NDI_SERVICE_TYPE));
    assert!(types.iter().any(|t| t == "_zow._tcp"));
    assert!(types.iter().any(|t| t == "_extra._udp.local."));
    // `_ndi._tcp` matches the built-in NDI type (`.local.`-tolerant): never
    // browsed twice.
    let ndi_count = types
        .iter()
        .filter(|t| {
            t.trim_end_matches('.')
                .trim_end_matches(".local")
                .trim_end_matches('.')
                == "_ndi._tcp"
        })
        .count();
    assert_eq!(ndi_count, 1, "no duplicated browse types: {types:?}");
}
