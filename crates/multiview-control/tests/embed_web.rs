//! The embedded web UI (feature `embed-web`) is served as the router fallback —
//! the SPA loads at `/` and on deep links, without shadowing the API surface.
//!
//! Compiled only with `--features embed-web` (the built `web/dist` is inlined).
#![cfg(feature = "embed-web")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use support::{body_bytes, harness, send, ADMIN_TOKEN};

fn raw_get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn spa_index_is_served_at_root() {
    let h = harness();
    let response = send(&h.router, raw_get("/")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/html"),
        "root should serve HTML, got {content_type:?}"
    );
    let body = String::from_utf8_lossy(&body_bytes(response).await).into_owned();
    assert!(
        body.to_ascii_lowercase().contains("<!doctype html")
            || body.to_ascii_lowercase().contains("<html"),
        "root should serve the SPA index document"
    );
}

#[tokio::test]
async fn spa_deep_link_serves_index_for_client_routing() {
    // A client-side route (no such asset) must return index.html (200), not 404,
    // so the BrowserRouter can own deep links / page reloads.
    let h = harness();
    let response = send(&h.router, raw_get("/layouts/new")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&body_bytes(response).await).into_owned();
    assert!(
        body.to_ascii_lowercase().contains("<!doctype html")
            || body.to_ascii_lowercase().contains("<html"),
        "a deep link should serve the SPA index for client routing"
    );
}

#[tokio::test]
async fn spa_fallback_does_not_shadow_the_api() {
    // The API is reachable (here: authenticated, returns a JSON list), proving the
    // SPA fallback runs only for unmatched routes — it never swallows /api/v1.
    let h = harness();
    let response = send(
        &h.router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/layouts")
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the API must still answer under the SPA fallback"
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("application/json"),
        "the API route returns JSON, not the SPA HTML; got {content_type:?}"
    );
}

#[tokio::test]
async fn spa_does_not_shadow_the_openapi_docs() {
    let h = harness();
    let response = send(&h.router, raw_get("/api/v1/openapi.json")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&body_bytes(response).await).into_owned();
    assert!(
        body.contains("openapi"),
        "the OpenAPI document must still be served, not shadowed by the SPA"
    );
}

/// Slice 6 (#292): the engine replaces the nginx UI site, with its caching:
/// a hashed asset is immutable for a year, `index.html` is `no-cache`, every
/// answer carries a strong `ETag` and `If-None-Match` gets `304`, text is
/// gzipped for a client that accepts it, and a missing hashed asset is a real
/// `404` (never `index.html` served as JavaScript).
#[tokio::test]
async fn spa_caching_etag_gzip_and_missing_assets() {
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let h = harness();
    let get = |path: &str, headers: &[(&str, &str)]| {
        let mut r = Request::builder().method("GET").uri(path);
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        r.body(Body::empty()).unwrap()
    };
    let index = send(&h.router, get("/", &[])).await;
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(index.headers()[header::CACHE_CONTROL], "no-cache");
    let etag = index.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.len() == 34, "{etag}");
    let plain = body_bytes(index).await;
    // Revalidation of index.html: 304, no body.
    let again = send(&h.router, get("/", &[("if-none-match", &etag)])).await;
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(again.headers()[header::CACHE_CONTROL], "no-cache");
    assert!(body_bytes(again).await.is_empty());
    // A deep link serves the same document with the same validator.
    let deep = send(&h.router, get("/layouts/new", &[])).await;
    assert_eq!(deep.headers()[header::ETAG].to_str().unwrap(), etag);

    // The largest hashed JS bundle: immutable, gzipped on request.
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/assets");
    let js = std::fs::read_dir(&dist)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "js"))
        .max_by_key(|e| e.metadata().unwrap().len())
        .unwrap();
    let name = js.file_name().into_string().unwrap();
    let on_disk = std::fs::read(js.path()).unwrap();
    let path = format!("/assets/{name}");
    let identity = send(&h.router, get(&path, &[])).await;
    assert_eq!(identity.status(), StatusCode::OK);
    assert_eq!(
        identity.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(identity.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .contains("javascript"));
    assert_eq!(identity.headers()[header::VARY], "Accept-Encoding");
    assert!(identity.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body_bytes(identity).await, on_disk);
    let gz = send(&h.router, get(&path, &[("accept-encoding", "gzip, br")])).await;
    assert_eq!(gz.headers()[header::CONTENT_ENCODING], "gzip");
    assert!(gz.headers()[header::ETAG].to_str().unwrap().ends_with("-gz\""));
    let compressed = body_bytes(gz).await;
    assert!(compressed.len() < on_disk.len() / 2);
    let mut inflated = Vec::new();
    GzDecoder::new(&compressed[..])
        .read_to_end(&mut inflated)
        .unwrap();
    assert_eq!(inflated, on_disk);

    // index.html is HTML: gzip applies only above the minimum size; either
    // way the decoded body equals the identity one.
    let index_gz = send(&h.router, get("/", &[("accept-encoding", "gzip")])).await;
    let encoded = index_gz.headers().get(header::CONTENT_ENCODING).is_some();
    let bytes = body_bytes(index_gz).await;
    if encoded {
        let mut out = Vec::new();
        GzDecoder::new(&bytes[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, plain);
    } else {
        assert_eq!(bytes, plain);
    }

    // A previous build's chunk: 404, not index.html.
    let stale = send(&h.router, get("/assets/index-OLDHASH.js", &[])).await;
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
    // HEAD carries the headers without a body.
    let head = send(
        &h.router,
        Request::builder()
            .method("HEAD")
            .uri(&path)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
        on_disk.len().to_string()
    );
    assert!(body_bytes(head).await.is_empty());
}
