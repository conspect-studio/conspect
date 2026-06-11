#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing contract test for the `input.streams`
//! realtime event (RT-3, ADR-0034 §9). The event carries an input's full
//! [`multiview_core::stream::StreamInventory`] so the API/UI can SHOW every
//! elementary stream an input offers. It rides the EXISTING `Topic::Inputs`
//! lane (a delta on re-probe / PMT-version bump) and, like every other realtime
//! frame, must be internally-tagged on `t` (never `untagged`) and survive a
//! JSON round-trip.

use multiview_core::stream::{
    StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};
use multiview_core::time::MediaTime;
use multiview_events::event::InputStreams;
use multiview_events::{Envelope, Event, EventEnvelope, Seq, Topic};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(920_451_123_456)
}

/// A small but representative inventory: one hard-keyed (TS PID) video stream
/// and one soft-keyed (general/libav) audio track flagged default.
fn sample_inventory() -> StreamInventory {
    let video = StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Video, 0x100),
        StreamKind::Video,
        "h264",
        StreamDetail::Video {
            width: 1920,
            height: 1080,
            frame_rate: None,
        },
    );
    let audio = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Audio, 0, "aac", None, None),
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels: 2,
            sample_rate: 48_000,
        },
    )
    .with_default(true);
    StreamInventory::from_streams(vec![video, audio]).with_input_id("cam1")
}

#[test]
fn input_streams_event_carries_the_inventory_and_routes_on_inputs() {
    let event = Event::InputStreams(InputStreams {
        input_id: "cam1".to_owned(),
        inventory: sample_inventory(),
    });

    // The wire discriminator + control classification.
    assert_eq!(event.type_tag(), "input.streams");
    assert!(
        !event.is_control(),
        "input.streams is a data event, not a control frame"
    );

    // Internally-tagged (`t`/`data`), never untagged: the Rust field name must
    // not leak and the discriminator must be present at the top level of the
    // payload.
    let v: Value = serde_json::to_value(&event).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.get("t").unwrap(), &json!("input.streams"));
    let data = obj.get("data").unwrap().as_object().unwrap();
    assert_eq!(data.get("input_id").unwrap(), &json!("cam1"));
    let inventory = data.get("inventory").unwrap().as_object().unwrap();
    let streams = inventory.get("streams").unwrap().as_array().unwrap();
    assert_eq!(streams.len(), 2, "both elementary streams survive the wire");
    // StreamKind is tagged (`kind`), never untagged.
    let first_kind = streams[0].as_object().unwrap().get("kind").unwrap();
    assert_eq!(first_kind, &json!("video"));

    // Survives a full JSON round-trip through the one tagged union.
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "input.streams must survive a JSON round-trip");
}

#[test]
fn input_streams_event_round_trips_through_the_envelope_on_topic_inputs() {
    let env: EventEnvelope = Envelope::new(
        Topic::Inputs,
        Seq::new(4242),
        ts(),
        Event::InputStreams(InputStreams {
            input_id: "cam1".to_owned(),
            inventory: sample_inventory(),
        }),
    )
    .with_id("cam1");

    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    // It rides the EXISTING inputs lane (no new topic).
    assert_eq!(obj.get("topic").unwrap(), &json!("inputs"));
    assert_eq!(obj.get("t").unwrap(), &json!("input.streams"));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "the envelope must round-trip on Topic::Inputs");
}

#[test]
fn unknown_input_streams_payload_is_rejected_not_untagged() {
    // A near-miss tag must hard-fail (tagged, never untagged fall-through).
    let bad =
        json!({"t": "input.streamz", "data": {"input_id": "x", "inventory": {"streams": []}}});
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err(), "an unknown discriminator must not parse");
}
