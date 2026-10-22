#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
//! RIST link-statistics telemetry surface tests (ADR-0095 Tier-1 / RIST-5).
//!
//! The `multiview-telemetry` `rist` module is the **model**: it registers the
//! Prometheus series for a RIST link's health (rtt / quality / bandwidth as
//! gauges; retransmits / lost / recovered / sent as cumulative counters) keyed by
//! a bounded `{link, role}` label set, and exposes a pure loss-assessment that the
//! producer maps to a `health.warning`. These tests prove a sample's numbers flow
//! into the registered series and that sustained low quality is reported (and
//! clears on recovery) — without ever touching the data plane.

use multiview_telemetry::metrics::{MetricKind, MetricsRegistry};
use multiview_telemetry::rist::{names, RistLinkGauges, RistLinkRole, RistLinkSample};

fn clean_sample(link: &str) -> RistLinkSample {
    RistLinkSample {
        link_id: link.to_owned(),
        role: RistLinkRole::Sender,
        flow_id: 7,
        cname: "egress".to_owned(),
        peer_count: 1,
        rtt_ms: 35,
        quality: 99.8,
        bandwidth_bps: 10_000_000,
        retry_bandwidth_bps: 5_000,
        sent: 1_000,
        received: 0,
        retransmitted: 6,
        lost: 0,
        recovered: 6,
        since: 1,
    }
}

#[test]
fn update_writes_every_link_series_into_the_registry() {
    let reg = MetricsRegistry::new();
    let mut gauges = RistLinkGauges::register(&reg, "out-rist", RistLinkRole::Sender);
    gauges.update(&clean_sample("out-rist"));

    // The gauges reflect the latest sample.
    assert_eq!(gauges.rtt_ms().get(), 35.0);
    assert_eq!(gauges.quality().get(), 99.8);
    assert_eq!(gauges.bandwidth_bps().get(), 10_000_000.0);

    // The cumulative counters reflect the sample's absolute counts. (librist
    // reports cumulative totals; the registry counter is set to the absolute
    // value, not incremented per call, so a re-poll is idempotent.)
    assert_eq!(gauges.retransmitted().get(), 6.0);
    assert_eq!(gauges.lost().get(), 0.0);
    assert_eq!(gauges.recovered().get(), 6.0);
    assert_eq!(gauges.sent().get(), 1_000.0);

    // The series are registered under the documented names with a {link, role}
    // label set, as gauges (cumulative-as-gauge, so a re-poll overwrites with the
    // latest absolute total rather than double-counting).
    let series = reg.series();
    let rtt = series
        .iter()
        .find(|s| s.name == names::RIST_LINK_RTT_MS)
        .expect("rtt series registered");
    assert_eq!(rtt.kind, MetricKind::Gauge);
    assert_eq!(rtt.labels.render(), r#"{link="out-rist",role="sender"}"#);

    assert!(series.iter().any(|s| s.name == names::RIST_LINK_QUALITY));
    assert!(series
        .iter()
        .any(|s| s.name == names::RIST_LINK_RETRANSMITTED));
    assert!(series.iter().any(|s| s.name == names::RIST_LINK_LOST));
    assert!(series.iter().any(|s| s.name == names::RIST_LINK_RECOVERED));
}

#[test]
fn re_polling_a_cumulative_total_is_idempotent_not_double_counted() {
    // librist hands cumulative totals; setting the same total twice must not
    // double it (a counter += would be a bug here).
    let reg = MetricsRegistry::new();
    let mut gauges = RistLinkGauges::register(&reg, "out-rist", RistLinkRole::Sender);
    let mut s = clean_sample("out-rist");
    s.retransmitted = 100;
    gauges.update(&s);
    gauges.update(&s);
    assert_eq!(gauges.retransmitted().get(), 100.0);
}

#[test]
fn clean_link_does_not_raise_a_loss_warning() {
    let mut gauges =
        RistLinkGauges::register(&MetricsRegistry::new(), "link-7", RistLinkRole::Sender);
    let assessment = gauges.update(&clean_sample("link-7"));
    assert!(
        !assessment.loss_warning_active(),
        "a high-quality link is healthy"
    );
}

#[test]
fn sustained_low_quality_raises_then_clears_the_loss_warning() {
    let mut gauges =
        RistLinkGauges::register(&MetricsRegistry::new(), "link-7", RistLinkRole::Sender);

    // A single dip below the quality floor is NOT yet a warning (hysteresis:
    // transient blips are expected — bad-inputs-are-the-purpose).
    let mut bad = clean_sample("link-7");
    bad.quality = 60.0;
    bad.lost = 50;
    let a1 = gauges.update(&bad);
    assert!(!a1.loss_warning_active(), "one dip is not yet sustained");

    // Sustained: a second consecutive low-quality sample raises the warning.
    let a2 = gauges.update(&bad);
    assert!(
        a2.loss_warning_active(),
        "sustained low quality is a warning"
    );
    assert!(a2.message().contains("link-7"), "names the link");

    // Recovery clears it.
    let a3 = gauges.update(&clean_sample("link-7"));
    assert!(!a3.loss_warning_active(), "recovery clears the warning");
}
