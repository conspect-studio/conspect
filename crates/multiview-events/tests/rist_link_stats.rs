#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing contract tests for the RIST link-statistics
//! wire types (ADR-0095 Tier-1 / RIST-5): the [`RistLinkStats`] sample and the
//! `rist.link.stats` [`Event`] variant, plus the sustained-loss
//! [`WarningCode::RistLinkLoss`] entry. These prove the new variants are
//! internally-tagged (`t`/`data`, never untagged), survive a JSON round-trip,
//! ride [`Topic::Outputs`] (a RIST egress link's health rides the outputs lane),
//! are conflated (latest-wins telemetry, ring-excluded), and that the link-health
//! fields (rtt/retransmits/quality/bandwidth/lost/recovered) are carried on wire.

use multiview_events::{
    Envelope, Event, EventEnvelope, RistLinkRole, RistLinkStats, Seq, Topic, WarningCode,
};
use serde_json::{json, Value};

fn sample_stats() -> RistLinkStats {
    RistLinkStats {
        link_id: "out-rist-primary".to_owned(),
        role: RistLinkRole::Sender,
        flow_id: 305_419_896,
        cname: "multiview-egress".to_owned(),
        peer_count: 1,
        rtt_ms: 42,
        quality: 99.5,
        bandwidth_bps: 12_000_000,
        retry_bandwidth_bps: 24_000,
        sent: 1_000_000,
        received: 0,
        retransmitted: 128,
        lost: 4,
        recovered: 120,
        since: 1_700_000_000,
    }
}

#[test]
fn rist_link_stats_event_roundtrips_and_carries_fields() {
    let event = Event::RistLinkStats(sample_stats());
    let value = serde_json::to_value(&event).unwrap();

    // Internally-tagged on `t` with the body under `data` (never untagged).
    assert_eq!(value["t"], json!("rist.link.stats"));
    let data = &value["data"];
    assert_eq!(data["link_id"], json!("out-rist-primary"));
    assert_eq!(data["role"], json!("sender"));
    assert_eq!(data["rtt_ms"], json!(42));
    assert_eq!(data["retransmitted"], json!(128));
    assert_eq!(data["lost"], json!(4));
    assert_eq!(data["recovered"], json!(120));
    assert_eq!(data["bandwidth_bps"], json!(12_000_000u64));
    assert!((data["quality"].as_f64().unwrap() - 99.5).abs() < 1e-9);

    let back: Event = serde_json::from_value(value).unwrap();
    assert_eq!(back, event);
}

#[test]
fn rist_link_stats_type_tag_matches() {
    let event = Event::RistLinkStats(sample_stats());
    assert_eq!(event.type_tag(), "rist.link.stats");
}

#[test]
fn rist_link_stats_is_conflated_latest_wins() {
    // A link-stats sample is a conflated telemetry snapshot: a re-snapshot heals
    // it, so it is excluded from the lossless replay ring (inv #10).
    let event = Event::RistLinkStats(sample_stats());
    assert!(event.is_conflated(), "rist link stats is latest-wins");
    assert!(!event.is_control());
}

#[test]
fn rist_link_stats_rides_outputs_topic_in_envelope() {
    // The event is carried on the `outputs` topic envelope (a RIST egress link's
    // health is an output-sink concern). The envelope round-trips losslessly.
    let env: EventEnvelope = Envelope::new(
        Topic::Outputs,
        Seq::new(7),
        multiview_core::time::MediaTime::from_nanos(1),
        Event::RistLinkStats(sample_stats()),
    );
    let value = serde_json::to_value(&env).unwrap();
    assert_eq!(value["topic"], json!("outputs"));
    assert_eq!(value["t"], json!("rist.link.stats"));

    let back: EventEnvelope = serde_json::from_value(value).unwrap();
    assert_eq!(back.payload, env.payload);
}

#[test]
fn rist_link_loss_warning_code_is_kebab_case() {
    // The sustained-loss health-warning code is a stable kebab-case wire string.
    let code = WarningCode::RistLinkLoss;
    assert_eq!(code.as_str(), "rist-link-loss");
    let value: Value = serde_json::to_value(code).unwrap();
    assert_eq!(value, json!("rist-link-loss"));
    let back: WarningCode = serde_json::from_value(json!("rist-link-loss")).unwrap();
    assert_eq!(back, code);
}

#[test]
fn rist_link_role_serialises_snake_case_both_directions() {
    assert_eq!(
        serde_json::to_value(RistLinkRole::Sender).unwrap(),
        json!("sender")
    );
    assert_eq!(
        serde_json::to_value(RistLinkRole::Receiver).unwrap(),
        json!("receiver")
    );
}
