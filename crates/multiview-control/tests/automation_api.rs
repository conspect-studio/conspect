//! ADR-0183 automation API: per-object authorization, audit, disarmed-by-default
//! persistence and the arm/disarm/restore surface.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::match_same_arms
)]
use axum::http::StatusCode;
use serde_json::json;
mod support;
use support::{
    body_json, get, harness, post_json, put_json, send, OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

async fn seed(router: &axum::Router) {
    for (id, body) in [
        (
            "news",
            json!({ "id": "news", "kind": "hls", "url": "https://example.com/a.m3u8" }),
        ),
        ("lineup", json!({ "id": "lineup", "kind": "bars" })),
    ] {
        let resp = send(
            router,
            post_json(
                &format!("/api/v1/sources/{id}"),
                OPERATOR_TOKEN,
                &json!({ "name": id, "body": body }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "seed {id}");
    }
}
fn rule_body() -> serde_json::Value {
    json!({ "rule": {
        "armed": true,
        "on_break_start": { "action": "swap_cell", "target": "bars" },
        "max_duration_s": 120
    } })
}

#[tokio::test]
async fn rule_round_trips_disarmed_then_arms_and_disarms() {
    let h = harness();
    seed(&h.router).await;
    let path = "/api/v1/inputs/news/events/automation";
    let resp = send(&h.router, get(path, VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["rule"].is_null());
    assert_eq!(body["bars_sources"], json!(["lineup"]));
    // PUT never arms, even when the body says armed=true.
    let resp = send(
        &h.router,
        put_json(path, OPERATOR_TOKEN, None, &rule_body()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["rule"]["armed"], false);
    assert_eq!(body["rule"]["max_duration_s"], 120);
    assert_eq!(body["rule"]["restore_on_end"], true);
    // Persisted through the canonical source document.
    let src = body_json(send(&h.router, get("/api/v1/sources/news", VIEWER_TOKEN)).await).await;
    assert_eq!(
        src["body"]["events"]["automation"]["on_break_start"]["target"],
        "bars"
    );
    let resp = send(
        &h.router,
        post_json(&format!("{path}/arm"), OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["rule"]["armed"], true);
    // An edit keeps the current armed state.
    let resp = send(
        &h.router,
        put_json(path, OPERATOR_TOKEN, None, &rule_body()),
    )
    .await;
    assert_eq!(body_json(resp).await["rule"]["armed"], true);
    let resp = send(
        &h.router,
        post_json(&format!("{path}/disarm"), OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(body_json(resp).await["rule"]["armed"], false);
    // Audit trail carries the arm and disarm commands.
    let audit = body_json(send(&h.router, get("/api/v1/audit", support::ADMIN_TOKEN)).await).await;
    let text = audit.to_string();
    assert!(text.contains("automation_arm") && text.contains("automation_disarm"));
    // Remove the rule.
    let resp = send(
        &h.router,
        put_json(path, OPERATOR_TOKEN, None, &json!({ "rule": null })),
    )
    .await;
    assert!(body_json(resp).await["rule"].is_null());
}

#[tokio::test]
async fn per_object_authorization_and_roles() {
    let h = harness();
    seed(&h.router).await;
    let path = "/api/v1/inputs/news/events/automation";
    // Scoped principal outside `news`: every verb is forbidden.
    assert_eq!(
        send(&h.router, get(path, SCOPED_TOKEN)).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&h.router, put_json(path, SCOPED_TOKEN, None, &rule_body()))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for verb in ["arm", "disarm", "restore"] {
        assert_eq!(
            send(
                &h.router,
                post_json(&format!("{path}/{verb}"), SCOPED_TOKEN, &json!({}))
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "{verb}"
        );
    }
    // Viewers may read but never write.
    assert_eq!(
        send(&h.router, put_json(path, VIEWER_TOKEN, None, &rule_body()))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &h.router,
            post_json(&format!("{path}/arm"), VIEWER_TOKEN, &json!({}))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    // Unknown source → 404; arm without a rule → 422; restore without executor → 503.
    assert_eq!(
        send(
            &h.router,
            get("/api/v1/inputs/ghost/events/automation", OPERATOR_TOKEN)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &h.router,
            post_json(&format!("{path}/arm"), OPERATOR_TOKEN, &json!({}))
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(
            &h.router,
            post_json(&format!("{path}/restore"), OPERATOR_TOKEN, &json!({}))
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    // Invalid rule (target == source) is rejected.
    let bad = json!({ "rule": { "on_break_start": { "action": "swap_cell", "target": "news" } } });
    assert_eq!(
        send(&h.router, put_json(path, OPERATOR_TOKEN, None, &bad))
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

/// Review R3-01: the GENERIC source create/replace authorizes the automation
/// it writes exactly as the dedicated route does. A scoped key that owns the
/// source but not the replacement target (or a named cell) gets 403, and
/// nothing is stored or submitted to the engine (so nothing is pinned).
#[tokio::test]
async fn generic_source_writes_authorize_the_rule_target_and_cells() {
    let mut h = harness();
    seed(&h.router).await;
    let _ = h.commands.try_drain();
    let owned = "/api/v1/sources/scoped-layout";
    let rule = |target: &str, cells: Option<Vec<&str>>| {
        let mut on = json!({ "action": "swap_cell", "target": target });
        if let Some(cells) = cells {
            on["cells"] = json!(cells);
        }
        json!({ "id": "scoped-layout", "kind": "bars",
            "events": { "automation": { "armed": true, "on_break_start": on } } })
    };
    // Create cannot bypass the guard either.
    let resp = send(
        &h.router,
        post_json(
            owned,
            SCOPED_TOKEN,
            &json!({ "name": "s", "body": rule("lineup", None) }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "create, foreign target"
    );
    assert_eq!(
        send(&h.router, get(owned, OPERATOR_TOKEN)).await.status(),
        StatusCode::NOT_FOUND,
        "a refused create stores nothing"
    );
    assert!(h.commands.try_drain().is_empty(), "nothing submitted");
    // The owner creates its source without a rule.
    let resp = send(
        &h.router,
        post_json(
            owned,
            SCOPED_TOKEN,
            &json!({ "name": "s", "body": { "id": "scoped-layout", "kind": "bars" } }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let _ = h.commands.try_drain();
    for (body, why) in [
        (rule("lineup", None), "foreign replacement target"),
        (
            rule("bars", Some(vec!["scoped-layout", "cell-elsewhere"])),
            "foreign cell",
        ),
    ] {
        let resp = send(
            &h.router,
            put_json(
                owned,
                SCOPED_TOKEN,
                Some("W/\"1\""),
                &json!({ "name": "s", "body": body }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{why}");
        let stored = send(&h.router, get(owned, OPERATOR_TOKEN)).await;
        assert_eq!(
            stored.headers()["etag"],
            "W/\"1\"",
            "{why}: nothing committed"
        );
        assert!(body_json(stored).await["body"]["events"].is_null(), "{why}");
        assert!(h.commands.try_drain().is_empty(), "{why}: nothing pinned");
    }
    // A rule naming only objects the key may act on (the built-in bars
    // target and its own id as a cell) is accepted.
    let resp = send(
        &h.router,
        put_json(
            owned,
            SCOPED_TOKEN,
            Some("W/\"1\""),
            &json!({ "name": "s", "body": rule("bars", Some(vec!["scoped-layout"])) }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}
