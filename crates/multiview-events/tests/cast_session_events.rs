#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde snapshot + round-trip + policy contract tests for the **ephemeral
//! cast-session lifecycle events** (DEV-D3.1, ADR-M011 over ADR-RT007).
//!
//! Cast-session list MEMBERSHIP changes (`POST /api/v1/cast/sessions`,
//! `DELETE /{id}`, the `/{id}/save` promotion) ride the same coarse `devices`
//! topic as the device lifecycle lane: `cast.session.started` /
//! `cast.session.removed` are **lossless low-rate lifecycle events** (never
//! conflated — a missed membership change is not healed by a re-snapshot of
//! the conflated `device.status` lane), internally tagged (`t`/`data`, never
//! untagged), scoped by the session id via the envelope `id`.

use multiview_core::time::MediaTime;
use multiview_events::{
    CastSessionRemoved, CastSessionStarted, Envelope, Event, EventEnvelope, SchemaVersion, Seq,
    Topic,
};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(920_451_123_456)
}

fn started() -> CastSessionStarted {
    CastSessionStarted {
        session_id: "cast-session-7e6e0c1a".to_owned(),
        name: Some("Lounge TV".to_owned()),
        address: "[2001:db8::20]:8009".to_owned(),
        output: "out-hls".to_owned(),
    }
}

#[test]
fn cast_session_started_envelope_matches_documented_shape() {
    let env: EventEnvelope = Envelope::new(
        Topic::Devices,
        Seq::new(41),
        ts(),
        Event::CastSessionStarted(started()),
    )
    .with_id("cast-session-7e6e0c1a");

    let v: Value = serde_json::to_value(&env).unwrap();
    assert_eq!(
        v,
        json!({
            "v": 1,
            "topic": "devices",
            "id": "cast-session-7e6e0c1a",
            "seq": 41,
            "ts": 920_451_123_456_i64,
            "t": "cast.session.started",
            "data": {
                "session_id": "cast-session-7e6e0c1a",
                "name": "Lounge TV",
                "address": "[2001:db8::20]:8009",
                "output": "out-hls"
            }
        })
    );

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env);
    assert_eq!(back.v, SchemaVersion::V1);
}

#[test]
fn cast_session_removed_envelope_matches_documented_shape() {
    let env: EventEnvelope = Envelope::new(
        Topic::Devices,
        Seq::new(42),
        ts(),
        Event::CastSessionRemoved(CastSessionRemoved::new("cast-session-7e6e0c1a")),
    )
    .with_id("cast-session-7e6e0c1a");

    let v: Value = serde_json::to_value(&env).unwrap();
    assert_eq!(
        v,
        json!({
            "v": 1,
            "topic": "devices",
            "id": "cast-session-7e6e0c1a",
            "seq": 42,
            "ts": 920_451_123_456_i64,
            "t": "cast.session.removed",
            "data": { "session_id": "cast-session-7e6e0c1a" }
        })
    );

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env);
}

#[test]
fn started_name_is_omitted_when_absent() {
    // An unnamed ad-hoc session must not serialize `"name": null` — the field
    // is skipped entirely (the same additive-optional posture as
    // `DeviceAdopted::name`).
    let mut payload = started();
    payload.name = None;
    let v = serde_json::to_value(Event::CastSessionStarted(payload)).unwrap();
    assert_eq!(v["t"], "cast.session.started");
    assert!(
        v["data"].get("name").is_none(),
        "an absent name must be omitted, got {v}"
    );
}

#[test]
fn cast_session_events_are_lossless_data_events() {
    // Membership changes are lossless lifecycle (replay-ring resident): a
    // dropped `cast.session.started` is NOT healed by re-snapshotting the
    // conflated status lane, so neither variant may be conflated. Neither is
    // a control frame.
    let started = Event::CastSessionStarted(started());
    let removed = Event::CastSessionRemoved(CastSessionRemoved::new("cast-session-1"));
    for (event, tag) in [
        (&started, "cast.session.started"),
        (&removed, "cast.session.removed"),
    ] {
        assert_eq!(event.type_tag(), tag);
        assert!(!event.is_control(), "{tag} must be a data event");
        assert!(
            !event.is_conflated(),
            "{tag} is lossless membership lifecycle, never conflated telemetry"
        );
    }
}

#[test]
fn asyncapi_document_covers_the_cast_session_lifecycle() {
    // The AsyncAPI 3.0 document (the committed `docs/api/asyncapi.json` is
    // regenerated from this, ADR-RT006) must describe both new messages: a
    // message entry each, a payload schema each, and an envelope `data.oneOf`
    // ref each — otherwise the realtime wire surface is undocumented and the
    // web `generate:events` types never see the payloads.
    let doc: Value =
        serde_json::from_str(&multiview_events::asyncapi::generate_asyncapi_document()).unwrap();

    let messages = doc["components"]["messages"]
        .as_object()
        .expect("components.messages");
    for name in ["CastSessionStarted", "CastSessionRemoved"] {
        assert!(
            messages.contains_key(name),
            "components.messages must contain {name}"
        );
        let schemas = doc["components"]["schemas"]
            .as_object()
            .expect("components.schemas");
        assert!(
            schemas.contains_key(name),
            "components.schemas must contain {name}"
        );
    }

    let one_of = doc["components"]["messages"]["Envelope"]["payload"]["properties"]["data"]
        ["oneOf"]
        .as_array()
        .expect("Envelope.payload data.oneOf");
    for name in ["CastSessionStarted", "CastSessionRemoved"] {
        let reference = format!("#/components/schemas/{name}");
        assert!(
            one_of
                .iter()
                .any(|entry| entry["$ref"].as_str() == Some(reference.as_str())),
            "Envelope data.oneOf must reference {reference}"
        );
    }
}
