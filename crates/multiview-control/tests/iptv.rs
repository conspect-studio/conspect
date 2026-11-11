//! IPTV publication (epg-playlist): durable `PUT /api/v1/iptv` round trip,
//! validation, per-object authorization, the public playlist/guide surface and
//! the M3U import endpoint.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::json;
use support::{
    body_bytes, body_json, etag, get, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    SCOPED_TOKEN, VIEWER_TOKEN,
};

const BOOT: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "cnn"
kind = "hls"
url = "https://user:secret@cdn.example.test/cnn/index.m3u8"
[sources.epg]
mode = "manual"
url = "https://epg.example.test/guide.xml"
channel_id = "464857"
[sources.playlist_metadata]
tvg_id = "CNN.us"
name = "CNN"
group = "News"
[[sources]]
id = "bars"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cnn"
[[outputs]]
id = "program"
kind = "hls"
path = "/tmp/iptv-test/program.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
id = "rtmp-out"
kind = "rtmp"
url = "rtmp://example.test/live"
codec = "h264"
"##;

async fn durable_state(dir: &std::path::Path) -> (AppState, std::path::PathBuf) {
    let path = dir.join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.clone(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )));
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let prospective: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, prospective, layouts)
        .await
        .unwrap();
    (state, path)
}

fn public(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(header::HOST, "mv.example.test:8080")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn publication_round_trips_durably_and_serves_public_files() {
    let dir = tempfile::tempdir().unwrap();
    let (state, path) = Box::pin(durable_state(dir.path())).await;
    let router = multiview_control::router(state);

    // Disabled by default: the public files 404, the API answers 200.
    assert_eq!(
        send(&router, public("/iptv/playlist.m3u8")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&router, public("/iptv/guide.xml")).await.status(),
        StatusCode::NOT_FOUND
    );
    let initial = send(&router, get("/api/v1/iptv", VIEWER_TOKEN)).await;
    assert_eq!(initial.status(), StatusCode::OK);
    let tag = etag(&initial).unwrap();
    assert_eq!(body_json(initial).await["config"]["enabled"], false);

    let doc = json!({
        "enabled": true,
        "entries": [
            {"kind": "output", "ref": "output-0", "name": "Program", "tvg_id": "mv.program"},
            {"kind": "source", "ref": "cnn", "allow_direct_source_url": true}
        ]
    });
    // If-Match is required.
    let missing = send(
        &router,
        put_json("/api/v1/iptv", OPERATOR_TOKEN, None, &doc),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::PRECONDITION_REQUIRED);
    // Viewers cannot write.
    let viewer = send(
        &router,
        put_json("/api/v1/iptv", VIEWER_TOKEN, Some(&tag), &doc),
    )
    .await;
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);
    let put = send(
        &router,
        put_json("/api/v1/iptv", OPERATOR_TOKEN, Some(&tag), &doc),
    )
    .await;
    assert_eq!(put.status(), StatusCode::OK);
    let fresh = etag(&put).unwrap();
    assert_ne!(fresh, tag);
    // A stale tag now conflicts.
    let outdated = send(
        &router,
        put_json("/api/v1/iptv", OPERATOR_TOKEN, Some(&tag), &doc),
    )
    .await;
    assert_eq!(outdated.status(), StatusCode::PRECONDITION_FAILED);

    // Persisted through the desired-state transaction (survives restart).
    let desired = load_desired(&path).unwrap().unwrap();
    let iptv = desired.config.iptv.unwrap();
    assert!(iptv.enabled);
    assert_eq!(iptv.entries.len(), 2);

    let playlist = send(&router, public("/iptv/playlist.m3u8")).await;
    assert_eq!(playlist.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(playlist).await).unwrap();
    assert!(
        text.starts_with("#EXTM3U url-tvg=\"http://mv.example.test:8080/iptv/guide.xml\""),
        "{text}"
    );
    assert!(
        text.contains("http://mv.example.test:8080/hls/program/program.m3u8"),
        "{text}"
    );
    assert!(text.contains("https://cdn.example.test/cnn/index.m3u8"));
    assert!(
        !text.contains("secret"),
        "credentials must never be published"
    );
    assert!(text.contains("tvg-id=\"CNN.us\""));

    // Guide: served (no upstream fetched in tests ⇒ synthesized channels), with
    // conditional GET and gzip variants.
    let guide = send(&router, public("/iptv/guide.xml")).await;
    assert_eq!(guide.status(), StatusCode::OK);
    let guide_tag = etag(&guide).unwrap();
    let xml = String::from_utf8(body_bytes(guide).await).unwrap();
    assert!(xml.contains("<channel id=\"mv.program\">"), "{xml}");
    assert!(xml.contains("<channel id=\"CNN.us\">"), "{xml}");
    let conditional = Request::builder()
        .uri("/iptv/guide.xml")
        .header(header::IF_NONE_MATCH, &guide_tag)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, conditional).await.status(),
        StatusCode::NOT_MODIFIED
    );
    let gz = send(&router, public("/iptv/guide.xml.gz")).await;
    assert_eq!(gz.status(), StatusCode::OK);
    assert_eq!(&body_bytes(gz).await[..2], &[0x1f, 0x8b]);
}

#[tokio::test]
async fn invalid_references_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = Box::pin(durable_state(dir.path())).await;
    let router = multiview_control::router(state);
    let tag = etag(&send(&router, get("/api/v1/iptv", ADMIN_TOKEN)).await).unwrap();
    for (doc, why) in [
        (
            json!({"enabled": true, "entries": [{"kind": "source", "ref": "nope", "allow_direct_source_url": true}]}),
            "unknown source",
        ),
        (
            json!({"enabled": true, "entries": [{"kind": "output", "ref": "output-9"}]}),
            "unknown output",
        ),
        (
            json!({"enabled": true, "entries": [{"kind": "output", "ref": "output-1"}]}),
            "non-HLS output",
        ),
        (
            json!({"enabled": true, "entries": [{"kind": "source", "ref": "cnn"}]}),
            "source without permission",
        ),
        (
            json!({"enabled": true, "entries": [{"kind": "source", "ref": "cnn", "allow_direct_source_url": true, "logo": "ftp://x"}]}),
            "bad logo",
        ),
        (json!({"enabled": true, "bogus": 1}), "unknown field"),
    ] {
        let response = send(
            &router,
            put_json("/api/v1/iptv", ADMIN_TOKEN, Some(&tag), &doc),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{why}");
    }
}

#[tokio::test]
async fn scoped_principals_cannot_publish_or_see_out_of_scope_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = Box::pin(durable_state(dir.path())).await;
    let router = multiview_control::router(state);
    let tag = etag(&send(&router, get("/api/v1/iptv", ADMIN_TOKEN)).await).unwrap();
    // SCOPED_TOKEN is an operator scoped to the single object id "scoped-layout".
    let doc = json!({"enabled": true, "entries": [{"kind": "source", "ref": "cnn", "allow_direct_source_url": true}]});
    let denied = send(
        &router,
        put_json("/api/v1/iptv", SCOPED_TOKEN, Some(&tag), &doc),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let ok = send(
        &router,
        put_json("/api/v1/iptv", ADMIN_TOKEN, Some(&tag), &doc),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);
    let tag = etag(&ok).unwrap();
    // The scoped reader sees no out-of-scope entries…
    let seen = body_json(send(&router, get("/api/v1/iptv", SCOPED_TOKEN)).await).await;
    assert_eq!(seen["config"]["entries"].as_array().unwrap().len(), 0);
    assert_eq!(seen["entries"].as_array().unwrap().len(), 0);
    // …and cannot clear the publication (which would unpublish `cnn`).
    let clear = json!({"enabled": false, "entries": []});
    let denied = send(
        &router,
        put_json("/api/v1/iptv", SCOPED_TOKEN, Some(&tag), &clear),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn deleting_a_published_source_drops_it_from_the_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = Box::pin(durable_state(dir.path())).await;
    let router = multiview_control::router(state);
    let tag = etag(&send(&router, get("/api/v1/iptv", ADMIN_TOKEN)).await).unwrap();
    let doc = json!({"enabled": true, "entries": [{"kind": "source", "ref": "bars", "allow_direct_source_url": true}, {"kind": "output", "ref": "output-0"}]});
    assert_eq!(
        send(
            &router,
            put_json("/api/v1/iptv", ADMIN_TOKEN, Some(&tag), &doc)
        )
        .await
        .status(),
        StatusCode::OK
    );
    let source = send(&router, get("/api/v1/sources/bars", ADMIN_TOKEN)).await;
    let source_tag = etag(&source).unwrap();
    let delete = Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/bars")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, source_tag)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, delete).await.status(), StatusCode::NO_CONTENT);
    let state = body_json(send(&router, get("/api/v1/iptv", ADMIN_TOKEN)).await).await;
    assert_eq!(
        state["entries"][0]["unavailable"],
        "source no longer exists"
    );
}

#[tokio::test]
async fn m3u_import_parses_pasted_text_and_requires_operator() {
    let harness = support::harness();
    let text = include_str!("fixtures/news-catalogue.m3u8");
    let viewer = send(
        &harness.router,
        post_json(
            "/api/v1/sources/import/m3u",
            VIEWER_TOKEN,
            &json!({"text": text}),
        ),
    )
    .await;
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);
    let response = send(
        &harness.router,
        post_json(
            "/api/v1/sources/import/m3u",
            OPERATOR_TOKEN,
            &json!({"text": text}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["url_tvg"][0], "https://epg.example.test/guide.xml.gz");
    assert_eq!(body["entries"].as_array().unwrap().len(), 4);
    assert_eq!(body["entries"][0]["tvg_id"], "CNN.us");
    for bad in [
        json!({}),
        json!({"text": "x", "url": "https://h/x"}),
        json!({"url": "file:///etc/passwd"}),
        json!({"url": "http://u:p@h/x"}),
    ] {
        let response = send(
            &harness.router,
            post_json("/api/v1/sources/import/m3u", OPERATOR_TOKEN, &bad),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
}

#[tokio::test]
async fn m3u_import_url_refuses_loopback_and_metadata_targets() {
    let harness = support::harness();
    for url in [
        "http://127.0.0.1/list.m3u",
        "http://[::1]/list.m3u",
        "http://169.254.169.254/latest/meta-data",
    ] {
        let response = send(
            &harness.router,
            post_json(
                "/api/v1/sources/import/m3u",
                OPERATOR_TOKEN,
                &json!({"url": url, "allow_private_network": true}),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{url}");
    }
}
