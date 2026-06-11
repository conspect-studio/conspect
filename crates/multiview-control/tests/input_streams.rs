//! End-to-end tests for the read-only input stream-inventory surface
//! (`GET /api/v1/inputs/{id}/streams`, RT-3 / ADR-0034 §9), driven through the
//! real router. The handler returns the input's
//! [`multiview_core::stream::StreamInventory`] from the **off-engine** cached
//! engine-state snapshot (invariant #10: it never touches the output-clock
//! thread — it reads the wait-free `LatestState` slot the engine publishes
//! into). Auth + BOLA mirror the sources routes; 404 when the input is unknown
//! / not yet probed; RFC 9457 problem on error.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{body_json, get, harness, send, OPERATOR_TOKEN, VIEWER_TOKEN};

/// Seed the engine-state snapshot the control plane republishes with a one-input
/// inventory under the conventional `inputs.<id>.streams` shape (RT-3: the
/// inventory is folded into the conflated `EngineStateSnapshot` blob, not a new
/// typed snapshot stream).
fn seed_snapshot_with_inventory(
    engine: &multiview_engine::EnginePublisher<serde_json::Value, multiview_events::Event>,
) {
    let snapshot = json!({
        "v": 1,
        "tick": 0,
        "pts_ns": 0,
        "canvas": { "width": 1920, "height": 1080 },
        "inputs": {
            "cam1": {
                "streams": {
                    "input_id": "cam1",
                    "streams": [
                        {
                            "id": { "kind_scope": "v", "key": "pid:256", "tier": "hard" },
                            "kind": "video",
                            "language": null,
                            "codec": "h264",
                            "title": null,
                            "default": false,
                            "detail": { "detail": "video", "params": { "width": 1920, "height": 1080, "frame_rate": null } }
                        },
                        {
                            "id": { "kind_scope": "a", "key": "pid:257", "tier": "hard" },
                            "kind": "audio",
                            "language": "eng",
                            "codec": "aac",
                            "title": null,
                            "default": true,
                            "detail": { "detail": "audio", "params": { "channels": 2, "sample_rate": 48000 } }
                        }
                    ]
                }
            }
        }
    });
    engine.publish_state(snapshot);
}

#[tokio::test]
async fn get_streams_returns_the_inventory_for_a_known_input() {
    let h = harness();
    seed_snapshot_with_inventory(&h.engine);

    let resp = send(
        &h.router,
        get("/api/v1/inputs/cam1/streams", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/json"
    );
    let inv = body_json(resp).await;
    // The response IS the StreamInventory: input_id + the full stream list.
    assert_eq!(inv["input_id"], "cam1");
    let streams = inv["streams"].as_array().expect("streams is an array");
    assert_eq!(streams.len(), 2, "every elementary stream is surfaced");
    assert_eq!(streams[0]["kind"], "video");
    assert_eq!(streams[1]["kind"], "audio");
    assert_eq!(streams[1]["default"], true);
    assert_eq!(streams[1]["language"], "eng");
}

#[tokio::test]
async fn get_streams_for_unknown_input_is_404_problem_json() {
    let h = harness();
    seed_snapshot_with_inventory(&h.engine);

    let resp = send(
        &h.router,
        get("/api/v1/inputs/missing/streams", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["type"], "/problems/not-found");
}

#[tokio::test]
async fn get_streams_before_any_snapshot_is_404() {
    // A freshly-started engine has published no snapshot yet: an input is simply
    // not-yet-probed, so the read is a 404 (not a 500/panic). The handler must
    // tolerate an absent snapshot.
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/inputs/cam1/streams", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 404);
}

#[tokio::test]
async fn get_streams_requires_authentication() {
    let h = harness();
    seed_snapshot_with_inventory(&h.engine);
    // No Authorization header: rejected before any data is read.
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/inputs/cam1/streams")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 401);
    assert_eq!(problem["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn viewer_may_read_streams() {
    // Discovery is a read: a read-only Viewer may list an input's streams.
    let h = harness();
    seed_snapshot_with_inventory(&h.engine);
    let resp = send(&h.router, get("/api/v1/inputs/cam1/streams", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_stream_inventory_mirror_matches_the_core_serde_shape() {
    // The OpenAPI schema mirror (StreamInventoryDoc) must serialise to the SAME
    // JSON shape as the real core StreamInventory it documents, across every
    // facet (video / audio / subtitle / data / timecode + soft/hard ids), or the
    // published contract would lie. We round-trip a real inventory's JSON THROUGH
    // the mirror and back and require byte-identical JSON.
    use multiview_control::openapi_schemas::StreamInventoryDoc;
    use multiview_core::stream::{
        Bcp47, DataKind, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory,
        StreamKind, TcSourceKind,
    };

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
        StableStreamId::from_general(StreamKind::Audio, 1, "aac", None, Some("Commentary")),
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels: 6,
            sample_rate: 48_000,
        },
    )
    .with_default(true)
    .with_title(Some("Commentary".to_owned()))
    .with_language(Bcp47::try_from("eng".to_owned()).ok());
    let subtitle = StreamDescriptor::new(
        StableStreamId::from_hls(StreamKind::Subtitle, "subs", "fra"),
        StreamKind::Subtitle,
        "webvtt",
        StreamDetail::Subtitle { forced: true },
    );
    let scte = StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Data(DataKind::Scte35), 0x1F4),
        StreamKind::Data(DataKind::Scte35),
        "scte_35",
        StreamDetail::Passthrough,
    );
    let tc = StreamDescriptor::new(
        StableStreamId::from_general(
            StreamKind::Timecode(TcSourceKind::AtcRp188),
            0,
            "tc",
            None,
            None,
        ),
        StreamKind::Timecode(TcSourceKind::AtcRp188),
        "timed_id3",
        StreamDetail::Passthrough,
    );
    let inv =
        StreamInventory::from_streams(vec![video, audio, subtitle, scte, tc]).with_input_id("cam1");

    let core_json = serde_json::to_value(&inv).unwrap();
    // The mirror parses the core JSON (same field names/tags)...
    let doc: StreamInventoryDoc = serde_json::from_value(core_json.clone()).unwrap();
    // ...and re-serialises to byte-identical JSON.
    let doc_json = serde_json::to_value(&doc).unwrap();
    assert_eq!(
        core_json, doc_json,
        "the OpenAPI mirror must match the core StreamInventory serde shape"
    );
}

#[tokio::test]
async fn scoped_operator_denied_other_input_is_403() {
    // BOLA: a principal scoped to a single object id may not read a different
    // input's streams (`authorize_object`), exactly like the sources routes.
    let h = harness();
    seed_snapshot_with_inventory(&h.engine);
    let resp = send(
        &h.router,
        get("/api/v1/inputs/cam1/streams", support::SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let problem = body_json(resp).await;
    assert_eq!(problem["status"], 403);
}
