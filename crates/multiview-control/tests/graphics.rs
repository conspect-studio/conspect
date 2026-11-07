//! `/api/v1/graphics/*`: data sources (with the weather alias), items,
//! channel rundown + playout verbs, per-object authorization and the
//! realtime `graphics.channel_state` event.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_events::Event;
use serde_json::json;
use support::{
    body_json, delete_if_match, get, harness_with, post_json, put_json, send, ADMIN_TOKEN,
    OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

fn sydney() -> serde_json::Value {
    json!({ "name": "Sydney", "kind": "bom_weather", "forecast_product": "IDN11060", "aac": "NSW_PT131" })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graphics_playout_end_to_end_with_authz_and_events() {
    let hub = multiview_graphics::GraphicsHub::new(multiview_config::GraphicsConfig::default());
    let h = harness_with(|state| state.with_graphics(hub.clone()));
    let mut events = h.engine.subscribe();
    let r = &h.router;

    // Templates are listed.
    let resp = send(r, get("/api/v1/graphics/templates", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let t = body_json(resp).await;
    assert!(t
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["id"] == "weather.seven_day"));

    // Data source: viewer cannot write; scoped operator cannot touch other ids.
    for token in [VIEWER_TOKEN, SCOPED_TOKEN] {
        let resp = send(
            r,
            put_json(
                "/api/v1/graphics/data-sources/sydney",
                token,
                None,
                &sydney(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{token}");
    }
    let resp = send(
        r,
        put_json(
            "/api/v1/graphics/data-sources/sydney",
            OPERATOR_TOKEN,
            None,
            &sydney(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    // The legacy weather alias sees it.
    let resp = send(r, get("/api/v1/weather/locations/sydney", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Unknown provider kind is 422.
    let resp = send(
        r,
        put_json(
            "/api/v1/graphics/data-sources/x",
            OPERATOR_TOKEN,
            None,
            &json!({ "name": "X", "kind": "rss" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Items.
    for (id, template) in [("syd7", "weather.seven_day"), ("sydnow", "weather.today")] {
        let resp = send(
            r,
            put_json(
                &format!("/api/v1/graphics/items/{id}"),
                OPERATOR_TOKEN,
                None,
                &json!({ "name": id, "template": template, "data_source": "sydney",
                         "style": { "background": "transparent", "accent": "#ffc94a" } }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }
    let resp = send(
        r,
        put_json(
            "/api/v1/graphics/items/bad",
            OPERATOR_TOKEN,
            None,
            &json!({ "name": "bad", "template": "weather.today", "data_source": "nowhere" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // Scoped principal sees no items (list drops out-of-scope rows) and is
    // denied a direct read.
    let resp = send(r, get("/api/v1/graphics/items", SCOPED_TOKEN)).await;
    assert_eq!(body_json(resp).await, json!([]));
    let resp = send(r, get("/api/v1/graphics/items/syd7", SCOPED_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // The data source is in use: delete is a 409.
    let resp = send(
        r,
        delete_if_match("/api/v1/graphics/data-sources/sydney", ADMIN_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Default channel exists; rundown PUT.
    let resp = send(r, get("/api/v1/graphics/channels", VIEWER_TOKEN)).await;
    let chans = body_json(resp).await;
    assert_eq!(chans[0]["id"], "gfx-channel-1");
    assert_eq!(chans[0]["preview_source"], "gfx-channel-1-preview");
    let resp = send(
        r,
        put_json(
            "/api/v1/graphics/channels/gfx-channel-1/rundown",
            OPERATOR_TOKEN,
            None,
            &json!({ "rundown": ["syd7", "sydnow"], "transition": { "kind": "mix", "frames": 8 } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        r,
        put_json(
            "/api/v1/graphics/channels/gfx-channel-1/rundown",
            OPERATOR_TOKEN,
            None,
            &json!({ "rundown": ["ghost"] }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Playout: scoped/viewer denied; take with nothing cued is 409.
    let take = "/api/v1/graphics/channels/gfx-channel-1/take";
    assert_eq!(
        send(r, post_json(take, VIEWER_TOKEN, &json!({})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(r, post_json(take, SCOPED_TOKEN, &json!({})))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(r, post_json(take, OPERATOR_TOKEN, &json!({})))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let resp = send(
        r,
        post_json(
            "/api/v1/graphics/channels/gfx-channel-1/next",
            OPERATOR_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(body_json(resp).await["preview"], "syd7");
    let resp = send(r, post_json(take, OPERATOR_TOKEN, &json!({}))).await;
    let v = body_json(resp).await;
    assert_eq!(v["program"], "syd7");
    assert_eq!(v["preview"], "sydnow");
    let resp = send(
        r,
        post_json(
            "/api/v1/graphics/channels/gfx-channel-1/cue",
            OPERATOR_TOKEN,
            &json!({ "item": "nope" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        hub.channel("gfx-channel-1").unwrap().program.as_deref(),
        Some("syd7")
    );

    // Deleting the on-air item cuts program to clear and leaves the rundown.
    let resp = send(
        r,
        delete_if_match("/api/v1/graphics/items/syd7", ADMIN_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(
        r,
        get("/api/v1/graphics/channels/gfx-channel-1", VIEWER_TOKEN),
    )
    .await;
    let v = body_json(resp).await;
    assert_eq!(v["program"], serde_json::Value::Null);
    assert_eq!(v["rundown"], json!(["sydnow"]));

    let resp = send(
        r,
        post_json(
            "/api/v1/graphics/channels/gfx-channel-1/clear",
            OPERATOR_TOKEN,
            &json!({ "transition": { "kind": "cut", "frames": 1 } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Realtime: next/take/... were published as graphics.channel_state.
    let mut actions = Vec::new();
    while let Ok(env) = events.try_recv() {
        if let Event::GraphicsChannelState(e) = &*env.event {
            actions.push(e.action.clone());
        }
    }
    assert!(actions.contains(&"next".to_owned()), "{actions:?}");
    assert!(actions.contains(&"take".to_owned()), "{actions:?}");
    assert!(actions.contains(&"rundown".to_owned()), "{actions:?}");

    // Thumbnails: the harness has no renderer → 503 (never a panic).
    let resp = send(
        r,
        get("/api/v1/graphics/items/sydnow/thumbnail.jpg", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
