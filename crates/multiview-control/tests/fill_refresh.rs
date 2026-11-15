//! Scheduled fill refresh endpoints (issue #472): per-object authorization on
//! status, and runtime schedule registration rules.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::fill_refresh::{FillOwner, FillSource, FillSpec, FirstDue};
use multiview_control::player_load::LoadThen;
use multiview_control::{InMemoryMediaPlayerStore, ResourceInput, ResourceRepository};
use serde_json::json;
use std::sync::Arc;
use support::{body_json, get, harness_with, send, OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN};

fn capture(fill: &str, player: &str) -> serde_json::Value {
    json!({"fill_id":fill,"target_player":player,"view":{"lat":-33.9,"lon":150.4,"zoom":8,"overlay":"radar","width":1280,"height":720,"fps":10,"frames":60,"public":true},"terms":{"accepted":true,"version":"windy-2023-09-01"}})
}
fn config_spec(fill: &str, player: &str) -> FillSpec {
    FillSpec {
        fill_id: fill.to_owned(),
        player: player.to_owned(),
        minutes: 5,
        then: LoadThen::Resume,
        body: serde_json::from_value(capture(fill, player)).unwrap(),
        owner: FillOwner::Declared,
        source: FillSource::Config,
    }
}
fn harness() -> support::Harness {
    let players = InMemoryMediaPlayerStore::new();
    for id in ["vt-1", "scoped-layout"] {
        players
            .create(
                id,
                ResourceInput {
                    name: id.to_owned(),
                    body: json!({"id": id, "eof_policy": "loop"}),
                },
            )
            .unwrap();
    }
    let players: Arc<dyn ResourceRepository> = Arc::new(players);
    harness_with(move |state| {
        state.fill_refresh.with_core(|core| {
            core.upsert(
                config_spec("radar", "vt-1"),
                1_800_000_000,
                FirstDue::NextSlot,
            )
            .unwrap();
            // Fill id in the scoped allowlist, player outside it.
            core.upsert(
                config_spec("scoped-layout", "vt-1"),
                1_800_000_000,
                FirstDue::NextSlot,
            )
            .unwrap();
        });
        state.with_media_player_store(players)
    })
}
fn request(
    method: &str,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    match body {
        Some(b) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

#[tokio::test]
async fn status_is_object_authorized_on_fill_and_player() {
    let h = harness();
    let r = send(&h.router, get("/api/v1/media/fills/refresh", VIEWER_TOKEN)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let all = body_json(r).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let radar = &all[0];
    assert_eq!(radar["fill_id"], "radar");
    assert_eq!(radar["in_flight"], false);
    assert_eq!(radar["source"], "config");
    assert_eq!(radar["during_refresh"], "play_last");
    assert!(radar["next_due"].is_string());
    assert!(radar["last_error"].is_null());

    // A scoped principal sees neither row: "radar" is outside its allowlist,
    // and "scoped-layout" targets a player outside it.
    let r = send(&h.router, get("/api/v1/media/fills/refresh", SCOPED_TOKEN)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await, json!([]));
    let r = send(
        &h.router,
        get("/api/v1/media/fills/radar/refresh", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = send(
        &h.router,
        get("/api/v1/media/fills/scoped-layout/refresh", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::NOT_FOUND,
        "foreign player is indistinguishable from absent"
    );
    let r = send(
        &h.router,
        get("/api/v1/media/fills/radar/refresh", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = send(
        &h.router,
        get("/api/v1/media/fills/none/refresh", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // reason: one linear walk of the registration contract
async fn runtime_schedule_registration_rules() {
    let h = harness();
    let ok = json!({"minutes": 10, "capture": capture("vt-1-fill", "vt-1")});
    // Viewer and scoped principals cannot register.
    for token in [VIEWER_TOKEN, SCOPED_TOKEN] {
        let r = send(
            &h.router,
            request(
                "PUT",
                "/api/v1/media/fills/vt-1-fill/refresh",
                token,
                Some(&ok),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
    // Body identity must match the path.
    let r = send(
        &h.router,
        request(
            "PUT",
            "/api/v1/media/fills/other/refresh",
            OPERATOR_TOKEN,
            Some(&ok),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Interval bounds.
    let bad = json!({"minutes": 0, "capture": capture("vt-1-fill", "vt-1")});
    let r = send(
        &h.router,
        request(
            "PUT",
            "/api/v1/media/fills/vt-1-fill/refresh",
            OPERATOR_TOKEN,
            Some(&bad),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Unknown player.
    let unknown = json!({"minutes": 10, "capture": capture("x", "vt-missing")});
    let r = send(
        &h.router,
        request(
            "PUT",
            "/api/v1/media/fills/x/refresh",
            OPERATOR_TOKEN,
            Some(&unknown),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // Config-declared fills are edited through config, not this API.
    let cfg = json!({"minutes": 10, "capture": capture("radar", "vt-1")});
    let r = send(
        &h.router,
        request(
            "PUT",
            "/api/v1/media/fills/radar/refresh",
            OPERATOR_TOKEN,
            Some(&cfg),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = send(
        &h.router,
        request(
            "DELETE",
            "/api/v1/media/fills/radar/refresh",
            OPERATOR_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CONFLICT);

    let r = send(
        &h.router,
        request(
            "PUT",
            "/api/v1/media/fills/vt-1-fill/refresh",
            OPERATOR_TOKEN,
            Some(&ok),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let st = body_json(r).await;
    assert_eq!(st["source"], "api");
    assert_eq!(st["minutes"], 10);
    assert_eq!(st["then"], "resume");
    assert_eq!(st["in_flight"], false);
    let r = send(
        &h.router,
        request(
            "DELETE",
            "/api/v1/media/fills/vt-1-fill/refresh",
            VIEWER_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = send(
        &h.router,
        request(
            "DELETE",
            "/api/v1/media/fills/vt-1-fill/refresh",
            OPERATOR_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = send(
        &h.router,
        get("/api/v1/media/fills/vt-1-fill/refresh", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
