//! The embedded single-page web UI (feature `embed-web`).
//!
//! The built SPA (`web/dist`, produced by the web build) is inlined into the
//! binary with [`rust_embed`] so the management UI ships self-contained — no
//! external file serving, same-origin with the API. [`spa_fallback`] is wired as
//! the control router's [`fallback`](axum::Router::fallback): it runs only for
//! requests no API/NMOS/docs route matched, so it can never shadow `/api/v1`,
//! `/x-nmos`, `/docs`, or `/api/v1/openapi.json`.
//!
//! Serving rules:
//! * a `GET` whose path maps to an embedded asset returns that asset with its
//!   guessed `Content-Type` (and a long immutable cache for the content-hashed
//!   `/assets/*` bundles Vite emits);
//! * any other `GET` returns `index.html` (HTTP 200) so the client-side router
//!   (`BrowserRouter`, HTML5 history) owns deep links like `/layouts/new`;
//! * a non-`GET` unmatched request is a genuine `404` (the SPA only serves
//!   reads; mutations live under `/api/v1`).
//!
//! If the UI was not built, `index.html` is absent and the fallback returns a
//! `503` explaining how to build it rather than a confusing blank `404`.

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The embedded production build of the web UI (`web/dist`).
///
/// `debug-embed` (see `Cargo.toml`) inlines the bytes in every profile, so this
/// is identical in debug and release and tests exercise the real assets. The
/// folder is resolved at compile time relative to this crate; the web build must
/// have produced `web/dist` first (the `embed-web` feature is off by default, so
/// the GPU-free default build never requires it).
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// The SPA index document, served for client-routed paths.
const INDEX_HTML: &str = "index.html";

/// Normalize a request path to an embedded-asset key: strip the leading `/`, and
/// map the site root to `index.html`.
fn asset_key(path: &str) -> &str {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        INDEX_HTML
    } else {
        trimmed
    }
}

/// Build a response serving an embedded file with its guessed content type. The
/// content-hashed `/assets/*` bundles are immutable, so they get a one-year
/// immutable cache; everything else (notably `index.html`) is `no-cache` so a
/// redeploy is picked up immediately.
fn serve_asset(key: &str) -> Option<Response> {
    let file = WebAssets::get(key)?;
    let mime = mime_guess::from_path(key).first_or_octet_stream();
    let cache = if key.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(file.data.into_owned()));
    // Building a response from embedded bytes + static header strings is
    // infallible; if it somehow failed we fall back to a plain 500 body.
    match response {
        Ok(ref mut resp) => {
            let headers = resp.headers_mut();
            if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
                headers.insert(header::CONTENT_TYPE, value);
            }
            if let Ok(value) = HeaderValue::from_str(cache) {
                headers.insert(header::CACHE_CONTROL, value);
            }
        }
        Err(_) => return Some(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
    response.ok()
}

/// The control router's fallback: serve the embedded SPA (asset-or-index for
/// `GET`, `404` otherwise). See the module docs for the exact rules.
pub async fn spa_fallback(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let key = asset_key(uri.path());

    // A direct asset hit (JS/CSS/index/favicon...).
    if let Some(response) = serve_asset(key) {
        return response;
    }

    // No asset matched: hand deep links to the client-side router by serving the
    // index document (HTTP 200). If the UI was never built, say so clearly.
    serve_asset(INDEX_HTML).unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "the web UI is not built into this binary; run the web build \
             (`npm --prefix web ci && npm --prefix web run build`) and rebuild \
             with `--features embed-web`",
        )
            .into_response()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn asset_key_maps_root_to_index() {
        assert_eq!(asset_key("/"), INDEX_HTML);
        assert_eq!(asset_key(""), INDEX_HTML);
        assert_eq!(asset_key("/assets/app.js"), "assets/app.js");
        assert_eq!(asset_key("/layouts/new"), "layouts/new");
    }

    #[test]
    fn index_html_is_embedded() {
        // The web build ran (CI / local), so the index document is present and
        // looks like an HTML document.
        let index = WebAssets::get(INDEX_HTML).expect("index.html must be embedded");
        let text = String::from_utf8_lossy(&index.data);
        assert!(
            text.to_ascii_lowercase().contains("<!doctype html")
                || text.to_ascii_lowercase().contains("<html"),
            "index.html should be an HTML document"
        );
    }
}
