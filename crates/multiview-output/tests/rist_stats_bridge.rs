#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
#![cfg(feature = "rist-stats")]
//! RIST stats bridge integration test (ADR-0095 Tier-1 / RIST-5).
//!
//! Proves a librist stats sample flows end-to-end through the output bridge:
//! into the telemetry metric series, out as a `rist.link.stats` wire event, and —
//! on sustained loss — as a `rist-link-loss` health warning that clears on
//! recovery. The librist session itself is exercised live only on hardware (the
//! `#[ignore]` test in `multiview-rist-sys`); here we feed the bridge samples
//! directly so the surfacing logic is fully testable with no native dep.

use multiview_events::{Event, RistLinkRole as WireRole, WarningCode};
use multiview_output::rist::RistStatsBridge;
use multiview_telemetry::metrics::MetricsRegistry;
use multiview_telemetry::rist::{RistLinkRole, RistLinkSample};

fn sample(link: &str, quality: f64, lost: u64) -> RistLinkSample {
    RistLinkSample {
        link_id: link.to_owned(),
        role: RistLinkRole::Sender,
        flow_id: 9,
        cname: "egress".to_owned(),
        peer_count: 1,
        rtt_ms: 30,
        quality,
        bandwidth_bps: 8_000_000,
        retry_bandwidth_bps: 2_000,
        sent: 5_000,
        received: 0,
        retransmitted: 10,
        lost,
        recovered: 8,
        since: 1,
    }
}

#[test]
fn a_sample_updates_telemetry_and_emits_a_stats_event() {
    let reg = MetricsRegistry::new();
    let mut bridge = RistStatsBridge::new(&reg, "out-rist", RistLinkRole::Sender);

    let events = bridge.ingest(&sample("out-rist", 99.0, 0));

    // The telemetry series carry the sample's numbers.
    assert_eq!(
        reg.series()
            .iter()
            .filter(|s| s.labels.render().contains(r#"link="out-rist""#))
            .count(),
        10,
        "all ten RIST link series are registered for the link"
    );

    // A `rist.link.stats` wire event is produced carrying the sample.
    let stats_event = events
        .iter()
        .find_map(|e| match e {
            Event::RistLinkStats(s) => Some(s),
            _ => None,
        })
        .expect("a rist.link.stats event is emitted");
    assert_eq!(stats_event.link_id, "out-rist");
    assert_eq!(stats_event.role, WireRole::Sender);
    assert_eq!(stats_event.rtt_ms, 30);
    assert_eq!(stats_event.retransmitted, 10);
    assert_eq!(stats_event.bandwidth_bps, 8_000_000);
    assert!((stats_event.quality - 99.0).abs() < 1e-9);

    // A clean link raises NO health warning.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::HealthWarningRaised(_))),
        "a clean link raises no warning"
    );
}

#[test]
fn sustained_loss_raises_then_clears_the_health_warning() {
    let reg = MetricsRegistry::new();
    let mut bridge = RistStatsBridge::new(&reg, "out-rist", RistLinkRole::Sender);

    // First dip: stats event, but not yet a warning (hysteresis).
    let e1 = bridge.ingest(&sample("out-rist", 50.0, 100));
    assert!(!e1
        .iter()
        .any(|e| matches!(e, Event::HealthWarningRaised(_))));

    // Sustained: the warning is raised, carrying the catalog code + remediation.
    let e2 = bridge.ingest(&sample("out-rist", 50.0, 200));
    let raised = e2
        .iter()
        .find_map(|e| match e {
            Event::HealthWarningRaised(w) => Some(w),
            _ => None,
        })
        .expect("sustained loss raises a health warning");
    assert_eq!(raised.code, WarningCode::RistLinkLoss);
    assert!(raised.active);
    assert!(raised.subsystem.contains("rist"));
    assert!(raised.message.contains("out-rist"));
    assert!(!raised.remediation.is_empty());

    // Re-raising while already active does NOT emit a duplicate raise (edge-only).
    let e3 = bridge.ingest(&sample("out-rist", 50.0, 300));
    assert!(
        !e3.iter()
            .any(|e| matches!(e, Event::HealthWarningRaised(_))),
        "no duplicate raise while the warning is already active"
    );

    // Recovery clears it exactly once.
    let e4 = bridge.ingest(&sample("out-rist", 99.0, 300));
    let cleared = e4
        .iter()
        .find_map(|e| match e {
            Event::HealthWarningCleared(w) => Some(w),
            _ => None,
        })
        .expect("recovery clears the warning");
    assert_eq!(cleared.code, WarningCode::RistLinkLoss);
    assert!(!cleared.active);
}
