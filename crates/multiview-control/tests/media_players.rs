//! Media-player transport operator-surface tests (tower oneshot), mirroring the
//! salvo arm/take/cancel surface (ADR-0097, ADR-RT008): the transport verbs
//! (`load`/`cue`/`play`/`pause`/`stop`/`seek`) and the vamp-exit triad
//! (`exit/arm`/`exit/take`/`exit/cancel`) each return `202 Accepted` + an
//! operation id that reaches the engine command bus, with per-object
//! authorization on the player id (BOLA — OWASP API1), `Idempotency-Key`
//! replay, the bounded bus shedding to `503` without blocking (invariant #10),
//! RBAC (operator can drive, viewer cannot), and `404` for an unknown player.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::{
    Command, InMemoryMediaPlayerStore, MediaTransportVerb, ResourceInput, ResourceRepository,
};
use serde_json::json;
use support::{
    body_json, get, harness_with, post_if_match, send, ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

/// BOLA ROW enumeration (OWASP API1, ADR-W005/ADR-W025): a media player is
/// object-scoped by its OWN id (`get_player` 403s an out-of-scope id), so
/// `list_players` MUST filter ROWS to the principal's allowlist — by the same
/// parity as `list_devices`. `SCOPED_TOKEN` is scoped to `["scoped-layout"]`.
#[tokio::test]
async fn list_filters_media_player_rows_to_the_scoped_allowlist() {
    let (h, _store) = harness_with_players(&["scoped-layout", "other-player"]);

    let resp = send(&h.router, get("/api/v1/media/players", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ids: Vec<String> = body_json(resp)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        vec!["scoped-layout".to_owned()],
        "a scoped principal must see ONLY its allowlisted media-player rows (BOLA)"
    );

    let resp = send(&h.router, get("/api/v1/media/players", ADMIN_TOKEN)).await;
    assert_eq!(body_json(resp).await.as_array().unwrap().len(), 2);
}

/// Build a harness whose media-player store is seeded with the given player
/// ids, returning the harness plus the store handle (so a test can assert on
/// it). Mirrors `seed_salvo` but for the config-derived media-player registry.
fn harness_with_players(ids: &[&str]) -> (support::Harness, Arc<dyn ResourceRepository>) {
    let store = InMemoryMediaPlayerStore::new();
    for id in ids {
        store
            .create(
                id,
                ResourceInput {
                    name: (*id).to_owned(),
                    body: json!({ "id": id, "eof_policy": "hold_last_frame" }),
                },
            )
            .expect("seed media player");
    }
    let store: Arc<dyn ResourceRepository> = Arc::new(store);
    let store_for_state = Arc::clone(&store);
    let h = harness_with(move |state| state.with_media_player_store(store_for_state));
    (h, store)
}

/// `POST /api/v1/media/players/{id}/play` returns `202` + an operation id and
/// the matching `Command::MediaTransport { verb: Play }` reaches the engine.
#[tokio::test]
async fn play_returns_202_and_reaches_the_engine() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);

    let resp = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-1/play", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let op = body["operation_id"].as_str().unwrap().to_owned();
    assert!(!op.is_empty());
    assert_eq!(body["kind"], "media_play");

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1);
    match &drained[0] {
        Command::MediaTransport {
            op: cmd_op,
            player,
            verb,
        } => {
            assert_eq!(cmd_op.as_str(), op);
            assert_eq!(player, "vt-1");
            assert_eq!(*verb, MediaTransportVerb::Play);
        }
        other => panic!("expected MediaTransport, got {other:?}"),
    }
}

/// `load` carries the asset id from the JSON body into the command verb.
#[tokio::test]
async fn load_carries_the_asset_from_the_body() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/media/players/vt-1/load")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "asset": "opener" }).to_string()))
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await["kind"], "media_load");

    let drained = h.commands.try_drain();
    match &drained[0] {
        Command::MediaTransport { player, verb, .. } => {
            assert_eq!(player, "vt-1");
            assert_eq!(
                *verb,
                MediaTransportVerb::Load {
                    asset: "opener".to_owned()
                }
            );
        }
        other => panic!("expected MediaTransport load, got {other:?}"),
    }
}

/// `cue` and `seek` carry an optional frame from the JSON body.
#[tokio::test]
async fn cue_and_seek_carry_an_optional_frame() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);

    // Cue with no body → frame None.
    let resp = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-1/cue", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Seek with an explicit frame.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/media/players/vt-1/seek")
        .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "frame": 240 }).to_string()))
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await["kind"], "media_seek");

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 2);
    match &drained[0] {
        Command::MediaTransport { verb, .. } => {
            assert_eq!(*verb, MediaTransportVerb::Cue { frame: None });
        }
        other => panic!("expected MediaTransport cue, got {other:?}"),
    }
    match &drained[1] {
        Command::MediaTransport { verb, .. } => {
            assert_eq!(*verb, MediaTransportVerb::Seek { frame: Some(240) });
        }
        other => panic!("expected MediaTransport seek, got {other:?}"),
    }
}

/// The vamp-exit triad mirrors salvo arm/take/cancel: each returns `202` +
/// op id, carries the player, and reaches the engine as its own variant.
#[tokio::test]
async fn exit_arm_take_cancel_return_202_and_reach_the_engine() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);

    let arm = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-1/exit/arm", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(arm.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(arm).await["kind"], "arm_media_exit");

    let take = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-1/exit/take", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(take.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(take).await["kind"], "take_media_exit");

    let cancel = send(
        &h.router,
        post_if_match(
            "/api/v1/media/players/vt-1/exit/cancel",
            OPERATOR_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(cancel.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(cancel).await["kind"], "cancel_media_exit");

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 3);
    assert!(matches!(
        &drained[0],
        Command::ArmMediaExit { player, .. } if player == "vt-1"
    ));
    assert!(matches!(
        &drained[1],
        Command::TakeMediaExit { player, .. } if player == "vt-1"
    ));
    assert!(matches!(
        &drained[2],
        Command::CancelMediaExit { player, .. } if player == "vt-1"
    ));
}

/// A transport verb against an UNKNOWN player is `404` and enqueues nothing —
/// the existence check fails fast before the engine bus (mirrors salvo 404).
#[tokio::test]
async fn transport_unknown_player_is_404_and_enqueues_nothing() {
    let (mut h, _store) = harness_with_players(&[]);
    let resp = send(
        &h.router,
        post_if_match("/api/v1/media/players/ghost/play", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["type"], "/problems/not-found");
    assert!(
        h.commands.try_drain().is_empty(),
        "an unknown player never reaches the engine"
    );
}

/// An exit verb against an UNKNOWN player is also `404` and enqueues nothing.
#[tokio::test]
async fn exit_arm_unknown_player_is_404_and_enqueues_nothing() {
    let (mut h, _store) = harness_with_players(&[]);
    let resp = send(
        &h.router,
        post_if_match("/api/v1/media/players/ghost/exit/arm", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(h.commands.try_drain().is_empty());
}

/// A viewer is read-only: it may NOT drive transport or arm an exit (403),
/// mirroring `viewer_may_read_but_may_not_take` for salvos.
#[tokio::test]
async fn viewer_may_not_drive_transport_or_arm_exit() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);

    for path in [
        "/api/v1/media/players/vt-1/play",
        "/api/v1/media/players/vt-1/exit/arm",
    ] {
        let resp = send(&h.router, post_if_match(path, VIEWER_TOKEN, None)).await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "a viewer is read-only and cannot drive {path}"
        );
        assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    }
    assert!(h.commands.try_drain().is_empty());
}

/// Per-object BOLA: an operator scoped to a single object id (`scoped-layout`)
/// is denied any OTHER player id and the command enqueues nothing — the deny is
/// at the HTTP boundary (OWASP API1; mirrors salvo per-object authorization).
#[tokio::test]
async fn object_scoped_operator_is_denied_a_player_outside_its_allowlist() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);

    let resp = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-1/play", SCOPED_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an object-scoped operator may not address a player outside its allowlist"
    );
    assert_eq!(body_json(resp).await["type"], "/problems/forbidden");
    assert!(
        h.commands.try_drain().is_empty(),
        "an out-of-scope transport never reaches the engine"
    );
}

/// `Idempotency-Key` replay: a retried key returns the original operation id
/// and enqueues exactly once (mirrors the salvo idempotency test).
#[tokio::test]
async fn play_replay_with_idempotency_key_enqueues_once() {
    let (mut h, _store) = harness_with_players(&["vt-1"]);
    let key = "media-play-key";

    let req = || -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/media/players/vt-1/play")
            .header(header::AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
            .header("idempotency-key", key)
            .body(Body::empty())
            .unwrap()
    };

    let resp1 = send(&h.router, req()).await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);
    let op1 = body_json(resp1).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp2 = send(&h.router, req()).await;
    assert_eq!(resp2.status(), StatusCode::ACCEPTED);
    let op2 = body_json(resp2).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(op1, op2, "a retried key returns the original operation id");

    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "the play was enqueued exactly once");
}

/// The bounded bus sheds a transport command to `503` without blocking, proving
/// the operator surface cannot back-pressure the engine (invariant #10).
#[tokio::test]
async fn transport_on_a_full_bus_sheds_to_503_without_blocking() {
    let store = InMemoryMediaPlayerStore::new();
    for id in ["vt-1", "vt-2"] {
        store
            .create(
                id,
                ResourceInput {
                    name: id.to_owned(),
                    body: json!({ "id": id, "eof_policy": "hold_last_frame" }),
                },
            )
            .expect("seed");
    }
    let store: Arc<dyn ResourceRepository> = Arc::new(store);
    let store_for_state = Arc::clone(&store);
    // Capacity 1, engine never drains.
    let h = support::harness_customized(1, move |state| {
        state.with_media_player_store(store_for_state)
    });

    let resp1 = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-1/play", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    let resp2 = send(
        &h.router,
        post_if_match("/api/v1/media/players/vt-2/play", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp2.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a full bus sheds the transport rather than blocking the engine"
    );
    assert_eq!(body_json(resp2).await["type"], "/problems/engine-busy");
}

/// A viewer MAY still read a player list/detail if a read surface exists; this
/// pins that the read path (if any) is at least not forbidden for a viewer.
/// (Transport/exit are write-only; this asserts the GET surface stays readable.)
#[tokio::test]
async fn list_players_is_readable_by_a_viewer() {
    let (h, _store) = harness_with_players(&["vt-1", "vt-2"]);
    let resp = send(&h.router, get("/api/v1/media/players", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let ids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["vt-1", "vt-2"]);
}
