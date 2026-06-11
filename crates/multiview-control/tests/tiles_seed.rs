//! Connect-time tiles seeding: a freshly-connected realtime client must receive
//! the CURRENT per-tile lifecycle state as a `tiles` `$snapshot` frame right
//! after `$hello` — without waiting for the next sparse `tile.state` delta —
//! whenever the engine's latest-state blob carries a `tiles` array.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use http_body_util::BodyExt;
use multiview_control::SessionStream;
use multiview_engine::EnginePublisher;
use multiview_events::{Event, FrameKind, LifecycleState, Topic};
use serde_json::json;
use support::{get, harness, send, VIEWER_TOKEN};

type Publisher = EnginePublisher<serde_json::Value, Event>;

/// An engine snapshot blob carrying per-tile lifecycle state, exactly as the
/// run loop's `state_snapshot` + `fold_tile_states` projection publishes it.
fn engine_blob_with_tiles() -> serde_json::Value {
    json!({
        "v": 1,
        "tick": 9,
        "pts_ns": 360_000_000_i64,
        "canvas": { "width": 1920, "height": 1080 },
        "tiles": [
            { "id": "cam1", "state": "LIVE" },
            { "id": "cam2", "state": "NO_SIGNAL" },
        ],
    })
}

#[tokio::test]
async fn tiles_snapshot_frame_is_built_from_the_engine_blob() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    engine.publish_state(engine_blob_with_tiles());
    let seq = engine.state.sequence();
    let snapshot = engine.state.latest().map(|arc| (*arc).clone()).unwrap();

    let mut session = SessionStream::new(engine.subscribe(), "sess-tiles", None);
    let hello = session.snapshot_frame(seq);
    assert_eq!(hello.envelope.seq.get(), 0);

    let frame = session
        .tiles_snapshot_frame(&snapshot, seq)
        .expect("a blob with tiles yields a snapshot frame");
    assert_eq!(frame.kind, FrameKind::Snapshot);
    assert_eq!(frame.envelope.topic, Topic::Tiles);
    assert_eq!(
        frame.envelope.seq.get(),
        1,
        "the tiles snapshot follows $hello on the per-connection cursor"
    );
    match &frame.envelope.payload {
        Event::TilesSnapshot(snap) => {
            assert_eq!(snap.as_of_seq, seq);
            assert_eq!(snap.tiles.len(), 2);
            assert_eq!(snap.tiles[0].id, "cam1");
            assert_eq!(snap.tiles[0].state, LifecycleState::Live);
            assert_eq!(snap.tiles[1].id, "cam2");
            assert_eq!(snap.tiles[1].state, LifecycleState::NoSignal);
        }
        other => panic!("expected Event::TilesSnapshot, got {other:?}"),
    }

    // The wire form carries the documented `$snapshot` + `tiles` discriminators.
    let v: serde_json::Value = serde_json::from_str(&frame.to_json().unwrap()).unwrap();
    assert_eq!(v["t"], "$snapshot");
    assert_eq!(v["topic"], "tiles");
    assert_eq!(v["data"]["tiles"][0]["state"], "LIVE");
}

#[tokio::test]
async fn tiles_snapshot_frame_tolerates_absent_or_malformed_tiles() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let mut session = SessionStream::new(engine.subscribe(), "sess-none", None);
    let _hello = session.snapshot_frame(0);

    // No `tiles` key (an older engine's blob): no frame.
    assert!(session
        .tiles_snapshot_frame(&json!({"v": 1, "tick": 0}), 0)
        .is_none());
    // `tiles` is not an array: no frame.
    assert!(session
        .tiles_snapshot_frame(&json!({"tiles": "nope"}), 0)
        .is_none());
    // No snapshot published yet (JSON null): no frame.
    assert!(session
        .tiles_snapshot_frame(&serde_json::Value::Null, 0)
        .is_none());
    // Malformed entries are skipped; well-formed ones survive.
    let mixed = json!({"tiles": [
        {"id": "ok", "state": "STALE"},
        {"id": 42, "state": "LIVE"},
        {"id": "bad-state", "state": "EXPLODED"},
    ]});
    let frame = session
        .tiles_snapshot_frame(&mixed, 7)
        .expect("one well-formed entry is enough");
    match &frame.envelope.payload {
        Event::TilesSnapshot(snap) => {
            assert_eq!(snap.tiles.len(), 1);
            assert_eq!(snap.tiles[0].id, "ok");
            assert_eq!(snap.tiles[0].state, LifecycleState::Stale);
        }
        other => panic!("expected Event::TilesSnapshot, got {other:?}"),
    }
}

/// End-to-end over the SSE transport: connect AFTER the engine published its
/// state, and the stream's SECOND `event: snapshot` frame carries the tiles —
/// the fresh-page seed (the bug this slice fixes: only `$hello` was sent).
#[tokio::test]
async fn sse_connect_emits_the_tiles_snapshot_after_hello() {
    let h = harness();
    h.engine.publish_state(engine_blob_with_tiles());

    let resp = send(&h.router, get("/api/v1/events", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Read streamed SSE chunks until both connect-time snapshot frames arrived
    // (the body never ends — it is a live stream — so read with a timeout).
    let mut body = resp.into_body();
    let mut text = String::new();
    let deadline = std::time::Duration::from_secs(5);
    while text.matches("event: snapshot").count() < 2 {
        let frame = tokio::time::timeout(deadline, body.frame())
            .await
            .expect("the connect-time snapshot frames must arrive promptly")
            .expect("the SSE stream must not end before the snapshots")
            .expect("the SSE stream must not error");
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }

    // Both frames are labelled `event: snapshot`; the first is $hello, the
    // second the tiles baseline.
    let payloads: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("snapshot data is JSON"))
        .collect();
    assert!(payloads.len() >= 2, "expected $hello + tiles snapshot");
    assert_eq!(payloads[0]["t"], "$hello");
    assert_eq!(payloads[1]["t"], "$snapshot");
    assert_eq!(payloads[1]["topic"], "tiles");
    assert_eq!(payloads[1]["data"]["tiles"][0]["id"], "cam1");
    assert_eq!(payloads[1]["data"]["tiles"][0]["state"], "LIVE");
    assert_eq!(payloads[1]["data"]["tiles"][1]["id"], "cam2");
    assert_eq!(payloads[1]["data"]["tiles"][1]["state"], "NO_SIGNAL");
}

/// A harness whose engine never published (no snapshot yet): the SSE stream
/// still opens and sends ONLY `$hello` — no malformed/empty tiles frame.
#[tokio::test]
async fn sse_connect_without_engine_tiles_sends_only_hello() {
    let h = harness();

    let resp = send(&h.router, get("/api/v1/events", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let mut body = resp.into_body();
    let mut text = String::new();
    // Read the first chunk(s): $hello arrives immediately; give the (absent)
    // tiles frame a short window to show up before asserting it did not.
    let deadline = std::time::Duration::from_millis(500);
    // Collect until a timeout (no more connect-time frames) or the stream
    // yields nothing further.
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(deadline, body.frame()).await {
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
    }
    assert_eq!(
        text.matches("event: snapshot").count(),
        1,
        "without engine tiles only the $hello snapshot is sent"
    );
    assert!(text.contains("$hello"));
    assert!(!text.contains("\"$snapshot\""));
}
