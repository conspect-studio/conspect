#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing contract tests for the **switcher** realtime
//! surface (ADR-RT008 / ADR-0097): the `Topic::Switcher` subscription lane and the
//! lossless `media.player_state` lifecycle event carrying a media player's discrete
//! transport state (the lifecycle a clip / VT-roll / opener player rides) plus its
//! `position_frames` playhead.
//!
//! These prove the new `Event` variant is internally-tagged (`t`/`data`, never
//! untagged), survives a JSON round-trip, carries the player via the envelope `id`,
//! is **lossless** (never conflated, on a topic that is not high-rate so it stays in
//! the replay ring per ADR-RT003), and that the `Vamping { exit_armed }` struct
//! variant — the VT vamp/exit delta (task #42) — round-trips on the wire. The state
//! enum mirrors how `SalvoPhase` is shaped/serialised (a small tagged enum).
//!
//! States covered: cued / playing / paused / stopped / `vamping{exit_armed}` / eof
//! / loading.

use multiview_core::time::MediaTime;
use multiview_events::event::{MediaPlayerEvent, MediaPlayerState};
use multiview_events::ordering::Accepted;
use multiview_events::{Envelope, Event, EventEnvelope, FrameKind, Seq, Topic, TopicCursor};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(1_234_567_890)
}

#[test]
fn switcher_topic_routes_and_is_not_high_rate() {
    // The `switcher` topic carries lossless lifecycle events scoped finer by the
    // envelope `id` (M/E, keyer, player, macro). It is NOT a high-rate conflated
    // lane — its events must stay in the lossless replay ring (ADR-RT003/RT008).
    assert_eq!(Topic::Switcher.as_str(), "switcher");
    assert!(!Topic::Switcher.is_control());
    assert!(!Topic::Switcher.is_high_rate());
}

#[test]
fn switcher_topic_roundtrips_on_the_wire() {
    let v = serde_json::to_value(Topic::Switcher).unwrap();
    assert_eq!(v, json!("switcher"));
    let back: Topic = serde_json::from_value(v).unwrap();
    assert_eq!(back, Topic::Switcher);
}

#[test]
fn media_player_state_event_wire_name_and_is_not_conflated() {
    let event = Event::MediaPlayerState(MediaPlayerEvent::new(
        "player:vt1",
        MediaPlayerState::Playing,
        4500,
    ));
    // Wire `t` discriminator is the shipped noun.verb form.
    assert_eq!(event.type_tag(), "media.player_state");
    // A discrete operator-meaningful lifecycle fact: a data event, never control,
    // and LOSSLESS — it must never be conflated so the replay ring keeps every
    // transition (ADR-RT008 "must never be conflated").
    assert!(!event.is_control());
    assert!(
        !event.is_conflated(),
        "media.player_state is a lossless lifecycle event, never conflated"
    );
}

#[test]
fn media_player_state_envelope_carries_player_via_id() {
    let env: EventEnvelope = Envelope::new(
        Topic::Switcher,
        Seq::new(9100),
        ts(),
        Event::MediaPlayerState(MediaPlayerEvent::new(
            "player:vt1",
            MediaPlayerState::Cued,
            0,
        )),
    )
    .with_id("player:vt1");

    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.get("t").unwrap(), &json!("media.player_state"));
    assert_eq!(obj.get("topic").unwrap(), &json!("switcher"));
    assert_eq!(obj.get("id").unwrap(), &json!("player:vt1"));
    assert!(
        !obj.contains_key("payload"),
        "Rust field name must not leak"
    );
    let data = obj.get("data").unwrap().as_object().unwrap();
    assert_eq!(data.get("player").unwrap(), &json!("player:vt1"));
    assert_eq!(data.get("position_frames").unwrap(), &json!(0));
    // The state is a tagged sub-object (`kind`), never untagged.
    let state = data.get("state").unwrap().as_object().unwrap();
    assert_eq!(state.get("kind").unwrap(), &json!("cued"));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(
        back, env,
        "media.player_state must survive a JSON round-trip"
    );
}

#[test]
fn all_simple_media_player_states_roundtrip_with_snake_case_kind() {
    // Each fieldless state serialises as a tagged sub-object `{ "kind": "<state>" }`
    // (snake_case), mirroring the SalvoPhase tagged-enum shape.
    let cases: Vec<(MediaPlayerState, &str)> = vec![
        (MediaPlayerState::Cued, "cued"),
        (MediaPlayerState::Playing, "playing"),
        (MediaPlayerState::Paused, "paused"),
        (MediaPlayerState::Stopped, "stopped"),
        (MediaPlayerState::Eof, "eof"),
        (MediaPlayerState::Loading, "loading"),
    ];
    for (state, wire) in cases {
        let event = Event::MediaPlayerState(MediaPlayerEvent::new("player:vt1", state, 12));
        let v = serde_json::to_value(&event).unwrap();
        let kind = v.pointer("/data/state/kind").unwrap();
        assert_eq!(kind, &json!(wire), "state kind mismatch for {wire}");
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, event, "{wire} state must round-trip");
    }
}

#[test]
fn vamping_state_carries_exit_armed_and_roundtrips() {
    // The VT vamp/exit delta (task #42): a Vamping state carries the exit-armed
    // latch. The struct variant must serialise tagged (`kind` = "vamping") with the
    // `exit_armed` field alongside it, and round-trip for both true and false.
    for armed in [true, false] {
        let event = Event::MediaPlayerState(MediaPlayerEvent::new(
            "player:vt1",
            MediaPlayerState::Vamping { exit_armed: armed },
            7200,
        ));
        let v = serde_json::to_value(&event).unwrap();
        let state = v.pointer("/data/state").unwrap().as_object().unwrap();
        assert_eq!(state.get("kind").unwrap(), &json!("vamping"));
        assert_eq!(
            state.get("exit_armed").unwrap(),
            &json!(armed),
            "exit_armed must ride the wire"
        );
        assert_eq!(v.pointer("/data/position_frames").unwrap(), &json!(7200u64));

        let back: Event = serde_json::from_value(v).unwrap();
        match &back {
            Event::MediaPlayerState(e) => {
                assert_eq!(e.state, MediaPlayerState::Vamping { exit_armed: armed });
            }
            other => panic!("expected MediaPlayerState, got {other:?}"),
        }
        assert_eq!(back, event, "vamping(exit_armed={armed}) must round-trip");
    }
}

#[test]
fn media_player_event_asset_is_optional_and_roundtrips() {
    // The optional asset id (the loaded clip) rides the wire when present and is
    // omitted when absent — mirroring SalvoEvent's optional `head` scope.
    let with_asset = MediaPlayerEvent::new("player:vt1", MediaPlayerState::Cued, 0)
        .with_asset("asset:opener_v3");
    let event = Event::MediaPlayerState(with_asset);
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v.pointer("/data/asset").unwrap(), &json!("asset:opener_v3"));
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "asset-scoped state must round-trip");

    // Absent asset is skipped on the wire (no `null`, no leaked Rust field).
    let no_asset = Event::MediaPlayerState(MediaPlayerEvent::new(
        "player:vt1",
        MediaPlayerState::Stopped,
        9,
    ));
    let v = serde_json::to_value(&no_asset).unwrap();
    assert!(
        v.pointer("/data/asset").is_none(),
        "an absent asset must be skipped on the wire"
    );
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, no_asset);
}

#[test]
fn unknown_media_player_state_kind_is_rejected() {
    // Tagged, never untagged: a near-miss state kind must hard-fail, not fall
    // through to some default.
    let bad = json!({
        "t": "media.player_state",
        "data": { "player": "player:vt1", "state": { "kind": "rewinding" }, "position_frames": 0 }
    });
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err());
}

#[test]
fn switcher_topic_obeys_snapshot_then_delta_with_resume() {
    // The switcher topic rides the same lossless snapshot ⊕ delta / resume-by-seq
    // contract (ADR-RT003): a baseline establishes state, deltas strictly advance,
    // gaps are reported, and a fresh snapshot rebuilds.
    let mut cur = TopicCursor::new(Topic::Switcher);
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(9000)).unwrap(),
        Accepted::SnapshotBaseline {
            seq: Seq::new(9000)
        }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(9001)).unwrap(),
        Accepted::Delta {
            seq: Seq::new(9001)
        }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(9004)).unwrap(),
        Accepted::DeltaWithGap {
            seq: Seq::new(9004),
            gap: 2
        }
    );
    assert!(cur.accept(FrameKind::Delta, Seq::new(9004)).is_err());
}

#[test]
fn switcher_topic_ordering_error_names_the_wire_topic() {
    let mut cur = TopicCursor::new(Topic::Switcher);
    let err = cur.accept(FrameKind::Delta, Seq::new(1)).unwrap_err();
    match err {
        multiview_events::Error::NonMonotonic { topic, .. } => assert_eq!(topic, "switcher"),
        other => panic!("wrong error: {other:?}"),
    }
}

// --- AsyncAPI generator coverage (ADR-RT006 drift gate) ---

#[test]
fn asyncapi_documents_the_media_player_state_message_and_schema() {
    use multiview_events::asyncapi;
    let doc: Value = serde_json::from_str(&asyncapi::generate_asyncapi_document()).unwrap();

    // The reusable message must exist and $ref its payload schema.
    let messages = doc
        .pointer("/components/messages")
        .and_then(Value::as_object)
        .expect("components.messages must exist");
    assert!(
        messages.contains_key("MediaPlayerEvent"),
        "components.messages must contain a MediaPlayerEvent message"
    );
    let payload_ref = doc
        .pointer("/components/messages/MediaPlayerEvent/payload/$ref")
        .and_then(Value::as_str)
        .expect("MediaPlayerEvent message must $ref its payload schema");
    assert_eq!(payload_ref, "#/components/schemas/MediaPlayerEvent");

    // The payload schema + the state enum schema must both be registered.
    for schema in &["MediaPlayerEvent", "MediaPlayerState"] {
        assert!(
            doc.pointer(&format!("/components/schemas/{schema}"))
                .is_some(),
            "components.schemas must contain `{schema}`"
        );
    }
    // The state union is discriminated by the `kind` property name (AsyncAPI 3.0
    // style: a string), never untagged.
    let discriminator = doc
        .pointer("/components/schemas/MediaPlayerState/discriminator")
        .and_then(Value::as_str)
        .expect("MediaPlayerState must declare a string discriminator");
    assert_eq!(discriminator, "kind");
}

#[test]
fn asyncapi_envelope_oneof_includes_media_player_event() {
    use multiview_events::asyncapi;
    let doc: Value = serde_json::from_str(&asyncapi::generate_asyncapi_document()).unwrap();
    let one_of = doc
        .pointer("/components/messages/Envelope/payload/properties/data/oneOf")
        .and_then(Value::as_array)
        .expect("Envelope data.oneOf must be an array");
    assert!(
        one_of
            .iter()
            .filter_map(|e| e.get("$ref").and_then(Value::as_str))
            .any(|r| r == "#/components/schemas/MediaPlayerEvent"),
        "the envelope data.oneOf must reference the MediaPlayerEvent schema"
    );
}

#[test]
fn asyncapi_topic_enum_includes_switcher() {
    use multiview_events::asyncapi;
    let doc: Value = serde_json::from_str(&asyncapi::generate_asyncapi_document()).unwrap();
    let topic_enum = doc
        .pointer("/components/messages/Envelope/payload/properties/topic/enum")
        .and_then(Value::as_array)
        .expect("envelope topic must be a string enum");
    assert!(
        topic_enum.iter().any(|v| v.as_str() == Some("switcher")),
        "the envelope topic enum must include `switcher`"
    );
}
