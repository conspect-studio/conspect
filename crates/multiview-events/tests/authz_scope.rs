#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // `event_from` takes the JSON body by value for call-site ergonomics; the
    // `json!` macro moves it, so a reference would only add noise in a test.
    clippy::needless_pass_by_value
)]
//! Golden classification table for [`multiview_events::Event::authz_scope`]
//! (ADR-W026): the unified, wildcard-free event scope model. Each case pins one
//! variant's authorization classification so the security-load-bearing arms
//! (`DeviceDiscovered → DiscoveryDomain`, `TimingStatus → Program`, the object
//! arms, and the explicit `Public` arms) can never silently drift.
//!
//! The events are constructed by deserializing their documented wire shape (the
//! enum is `#[non_exhaustive]`, so struct literals are unavailable from this
//! external test crate; deserialization is the honest cross-crate construction
//! path and also proves the wire compat for the new `domain` field).

use multiview_core::stream::StreamInventory;
use multiview_events::{
    AuthzScope, DeviceDiscovered, Event, InputConnection, InputStreams, LifecycleState,
    RistLinkRole, RistLinkStats, SalvoEvent, SalvoPhase,
};
use serde_json::json;

fn event_from(t: &str, data: serde_json::Value) -> Event {
    serde_json::from_value(json!({ "t": t, "data": data })).expect("event wire shape parses")
}

#[test]
fn device_discovered_with_domain_is_discovery_domain_labelled() {
    let e = event_from(
        "device.discovered",
        json!({
            "driver": "zowietek",
            "address": "http://[fd00:db8::42]",
            "family": "ipv6",
            "name": "ZowieBox 4K",
            "domain": "site-a"
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::DiscoveryDomain(Some("site-a")));
}

#[test]
fn device_discovered_without_domain_is_discovery_domain_none() {
    // An unlabelled row: fail-closed (a scoped principal must not see it). The
    // classifier reports the absence honestly; the policy lives in auth.rs.
    let e = event_from(
        "device.discovered",
        json!({
            "driver": "zowietek",
            "address": "http://[fd00:db8::42]",
            "family": "ipv6"
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::DiscoveryDomain(None));
}

#[test]
fn device_discovered_domain_survives_round_trip() {
    let e = DeviceDiscovered::new(
        "zowietek".to_owned(),
        "http://[fd00:db8::42]".to_owned(),
        multiview_events::AddressFamily::Ipv6,
    )
    .with_name("ZowieBox 4K".to_owned())
    .with_domain("site-a".to_owned());
    let wire = serde_json::to_value(&e).unwrap();
    assert_eq!(wire["domain"], json!("site-a"));
    let back: DeviceDiscovered = serde_json::from_value(wire).unwrap();
    assert_eq!(back.domain.as_deref(), Some("site-a"));
}

#[test]
fn device_discovered_domain_absent_is_omitted_on_wire() {
    let e = DeviceDiscovered::new(
        "zowietek".to_owned(),
        "http://[fd00:db8::42]".to_owned(),
        multiview_events::AddressFamily::Ipv6,
    );
    let wire = serde_json::to_value(&e).unwrap();
    assert!(
        wire.get("domain").is_none(),
        "unset domain must be omitted (skip_serializing_if), got {wire}"
    );
}

#[test]
fn timing_status_is_program_scoped_by_stream_id() {
    let e = event_from(
        "timing.status",
        json!({
            "stream_id": "prog-main",
            "epoch": {
                "wall_at_anchor_ns": 1_765_432_100_000_000_000_i64,
                "media_at_anchor": 900_000,
                "rate": { "num": 90_000, "den": 1 }
            },
            "link_offset_ns": 150_000_000,
            "clock_source": "ptp",
            "clock_quality": "locked",
            "groups": []
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::Program("prog-main"));
}

#[test]
fn tile_state_with_input_is_object_scoped() {
    let e = event_from(
        "tile.state",
        json!({
            "from": "RECONNECTING",
            "to": "NO_SIGNAL",
            "input": "input:ndi3",
            "trigger": "nosignal_timeout"
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::Object("input:ndi3"));
}

#[test]
fn tile_state_without_input_is_public() {
    // A placeholder tile carries no authorizable object — it stays public
    // (structurally id-less), exactly as under the pre-W026 firehose.
    let e = event_from(
        "tile.state",
        json!({
            "from": "LIVE",
            "to": "NO_SIGNAL",
            "trigger": "nosignal_timeout"
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::Public);
}

#[test]
fn media_player_state_is_object_scoped_by_player() {
    let e = event_from(
        "media.player_state",
        json!({ "player": "player:vt1", "state": { "kind": "playing" }, "position_frames": 0 }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::Object("player:vt1"));
}

#[test]
fn cast_session_started_is_object_scoped_by_session_id() {
    let e = event_from(
        "cast.session.started",
        json!({
            "session_id": "cast-session-7e6e0c1a",
            "name": "Lounge TV",
            "address": "[2001:db8::20]:8009",
            "output": "out-hls"
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::Object("cast-session-7e6e0c1a"));
}

#[test]
fn device_status_is_object_scoped_by_device_id() {
    let e = event_from(
        "device.status",
        json!({
            "device_id": "dev-foyer-decoder",
            "state": "ONLINE",
            "mode": "decoder",
            "capabilities": {
                "encode": false, "decode": true, "display": true,
                "sync": "offset-only", "audio": true, "reboot": true,
                "firmware_update": false
            },
            "streams": [],
            "last_seen_ts": 920_451_123_456_i64
        }),
    );
    assert_eq!(e.authz_scope(), AuthzScope::Object("dev-foyer-decoder"));
}

#[test]
fn control_and_telemetry_events_are_public() {
    // A representative sample of the explicitly-classified `Public` arms: the
    // firehose is now an enumerated, reviewed decision, not a `_ => None`
    // fallthrough.
    let ping: Event = serde_json::from_value(json!({ "t": "$ping" })).unwrap();
    assert_eq!(ping.authz_scope(), AuthzScope::Public);

    let output_status = event_from("output.status", json!({ "state": "running" }));
    assert_eq!(output_status.authz_scope(), AuthzScope::Public);
}

// ---- ADR-W026 leak-set tightenings ---------------------------------------
// The pre-W026 firehose drafted these object/output-bearing events as `Public`
// despite their REST twins being BOLA/per-output gated. Each case pins the
// CORRECT (tightened) classification so the exhaustive `authz_scope()` match can
// never silently re-ratify the leak. (Cross-vendor auth review, audits
// a0ac35bd + a13540b3.)

#[test]
fn rist_link_stats_is_output_scoped_by_link_id() {
    // `link_id` is the configured RIST source/output id (output axis); `cname`
    // leaks peer-hostname topology, so it must not firehose to output-scoped
    // principals. REST twin: RIST stats ride `Topic::Outputs`.
    let e = Event::RistLinkStats(RistLinkStats {
        link_id: "srt-egress-1".to_owned(),
        role: RistLinkRole::Sender,
        flow_id: 42,
        cname: "peer.example.invalid".to_owned(),
        peer_count: 1,
        rtt_ms: 20,
        quality: 100.0,
        bandwidth_bps: 5_000_000,
        retry_bandwidth_bps: 0,
        sent: 1_000,
        received: 0,
        retransmitted: 0,
        lost: 0,
        recovered: 0,
        since: 1,
    });
    assert_eq!(e.authz_scope(), AuthzScope::Output("srt-egress-1"));
}

#[test]
fn input_streams_is_object_scoped_by_input_id() {
    // Same id namespace `TileState` is object-scoped on; the REST twin
    // `GET /inputs/{id}/streams` is `authorize_object`-gated.
    let e = Event::InputStreams(InputStreams::new(
        "input:cam2",
        StreamInventory::from_streams(vec![]),
    ));
    assert_eq!(e.authz_scope(), AuthzScope::Object("input:cam2"));
}

#[test]
fn input_connection_is_object_scoped_by_input_id() {
    // An input's connect/flap lifecycle is the same leak class as its stream
    // inventory: "input cam2 flapping" must not reach a cam1-scoped principal.
    let e = Event::InputConnection(InputConnection::new(
        "input:cam2",
        LifecycleState::Reconnecting,
    ));
    assert_eq!(e.authz_scope(), AuthzScope::Object("input:cam2"));
}

#[test]
fn input_connection_carries_input_id_round_trip() {
    // The new object-axis field must survive the wire so the classifier can read
    // it (the event is un-attributable without it).
    let e = InputConnection::new("input:cam2", LifecycleState::Live).with_attempt(3);
    let wire = serde_json::to_value(&e).unwrap();
    assert_eq!(wire["input_id"], json!("input:cam2"));
    let back: InputConnection = serde_json::from_value(wire).unwrap();
    assert_eq!(back.input_id, "input:cam2");
    assert_eq!(back.attempt, Some(3));
}

#[test]
fn salvo_armed_with_head_is_object_and_output() {
    // The REST twin gates BOTH axes: `authorize_object(salvo)` +
    // `authorize_output(head)`. A single-axis scope under-gates; the conjunction
    // is the faithful classification.
    let e = Event::SalvoArmed(SalvoEvent::new("preset-a", SalvoPhase::Armed).with_head("out-1"));
    assert_eq!(
        e.authz_scope(),
        AuthzScope::ObjectAndOutput {
            object: "preset-a",
            output: "out-1"
        }
    );
}

#[test]
fn salvo_cancelled_with_head_is_object_and_output() {
    let e = Event::SalvoCancelled(
        SalvoEvent::new("preset-a", SalvoPhase::Cancelled).with_head("out-2"),
    );
    assert_eq!(
        e.authz_scope(),
        AuthzScope::ObjectAndOutput {
            object: "preset-a",
            output: "out-2"
        }
    );
}

#[test]
fn salvo_taken_without_head_is_object_scoped() {
    // No head → the REST twin skips `authorize_output`, so the whole-wall recall
    // is gated by the salvo object only (matches `submit_for_existing_salvo`).
    let e = Event::SalvoTaken(SalvoEvent::new("preset-a", SalvoPhase::Taken));
    assert_eq!(e.authz_scope(), AuthzScope::Object("preset-a"));
}
