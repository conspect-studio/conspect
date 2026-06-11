#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Wire-contract tests for the connect-time `tiles` `$snapshot` frame
//! (docs/api/realtime.md §5): the full current per-tile lifecycle baseline a
//! freshly-connected client rebuilds its tile cache from, before any sparse
//! `tile.state` delta arrives.

use multiview_core::time::MediaTime;
use multiview_events::{
    Envelope, Event, EventEnvelope, LifecycleState, Seq, TileSnapshotEntry, TilesSnapshot, Topic,
};
use serde_json::{json, Value};

fn sample() -> TilesSnapshot {
    TilesSnapshot {
        as_of_seq: 43,
        tiles: vec![
            TileSnapshotEntry {
                id: "cam1".to_owned(),
                state: LifecycleState::Live,
                input: Some("cam1".to_owned()),
            },
            TileSnapshotEntry {
                id: "cam2".to_owned(),
                state: LifecycleState::NoSignal,
                input: None,
            },
        ],
    }
}

#[test]
fn tiles_snapshot_roundtrips_through_json() {
    let env: EventEnvelope = Envelope::new(
        Topic::Tiles,
        Seq::new(1),
        MediaTime::from_nanos(920_000_112_500),
        Event::TilesSnapshot(sample()),
    );
    let text = serde_json::to_string(&env).unwrap();
    let back: EventEnvelope = serde_json::from_str(&text).unwrap();
    assert_eq!(env, back, "tiles snapshot must survive a JSON round-trip");
}

#[test]
fn tiles_snapshot_wire_shape_matches_realtime_md() {
    // The documented contract (realtime.md §5): `t` is `$snapshot`, the topic is
    // `tiles`, and `data` is `{as_of_seq, tiles:[{id, state, input?}, …]}` with
    // the lifecycle state as the SCREAMING_SNAKE wire string.
    let env: EventEnvelope = Envelope::new(
        Topic::Tiles,
        Seq::new(1),
        MediaTime::from_nanos(43),
        Event::TilesSnapshot(sample()),
    );
    let v: Value = serde_json::to_value(&env).unwrap();
    assert_eq!(v["t"], json!("$snapshot"));
    assert_eq!(v["topic"], json!("tiles"));
    assert_eq!(v["data"]["as_of_seq"], json!(43));
    assert_eq!(v["data"]["tiles"][0]["id"], json!("cam1"));
    assert_eq!(v["data"]["tiles"][0]["state"], json!("LIVE"));
    assert_eq!(v["data"]["tiles"][0]["input"], json!("cam1"));
    assert_eq!(v["data"]["tiles"][1]["state"], json!("NO_SIGNAL"));
    // An absent input is omitted, not serialized as null.
    assert!(
        v["data"]["tiles"][1]
            .as_object()
            .unwrap()
            .get("input")
            .is_none(),
        "a None input must be omitted from the wire entry"
    );
}

#[test]
fn tiles_snapshot_tag_is_snapshot_and_not_a_control_frame() {
    let event = Event::TilesSnapshot(sample());
    assert_eq!(event.type_tag(), "$snapshot");
    // `$snapshot` rides its data topic (`tiles`), never `$control` — a client
    // discriminates it by topic + `t` (realtime.md §5).
    assert!(
        !event.is_control(),
        "$snapshot is a per-topic baseline frame, not a $control frame"
    );
}

#[test]
fn asyncapi_document_declares_the_tiles_snapshot() {
    let doc: Value =
        serde_json::from_str(&multiview_events::asyncapi::generate_asyncapi_document()).unwrap();
    assert!(
        doc.pointer("/components/messages/TilesSnapshot").is_some(),
        "components.messages must declare TilesSnapshot"
    );
    assert!(
        doc.pointer("/components/schemas/TilesSnapshot").is_some(),
        "components.schemas must declare TilesSnapshot"
    );
    assert!(
        doc.pointer("/components/schemas/TileSnapshotEntry")
            .is_some(),
        "components.schemas must declare TileSnapshotEntry"
    );
    let one_of = doc
        .pointer("/components/messages/Envelope/payload/properties/data/oneOf")
        .and_then(Value::as_array)
        .expect("envelope data oneOf");
    assert!(
        one_of
            .iter()
            .any(|r| r["$ref"] == json!("#/components/schemas/TilesSnapshot")),
        "the envelope data oneOf must reference TilesSnapshot"
    );
}
