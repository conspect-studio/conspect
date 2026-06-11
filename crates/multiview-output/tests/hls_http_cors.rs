//! DEV-D1: the HLS delivery router's header contract (ADR-0032 §6).
//!
//! A Cast receiver (a browser app on a Google origin) and any browser player
//! fetch playlists/segments/init **cross-origin**, so every HLS delivery
//! endpoint must answer the CORS contract: reflect the request `Origin` (never
//! a hardcoded wildcard) with `Vary: Origin`, advertise `GET, HEAD, OPTIONS`,
//! allow `Content-Type, Range, Accept-Encoding`, expose
//! `Content-Length, Content-Range`, answer preflight `OPTIONS` with `204`, and
//! attach **no** CORS headers when the request carries no `Origin` (normal
//! players). Range requests keep working under CORS (segments are
//! range-served: `206` + `Content-Range`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::body::Body;
use axum::http::{header, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use multiview_output::hls::http::{byte_range, hls_router, ByteRange};
use proptest::prelude::*;
use tower::util::ServiceExt;

/// Segment fixture bytes (16 bytes so range maths are easy to eyeball).
const SEGMENT_BYTES: &[u8] = b"0123456789abcdef";

/// A playlist fixture body.
const PLAYLIST_TEXT: &str = "#EXTM3U\n#EXT-X-VERSION:7\n";

/// The cross-origin caller used throughout (a Cast receiver origin shape).
const ORIGIN: &str = "https://receiver.example";

/// Build an HLS output directory holding a playlist, a TS segment, a CMAF
/// part/init pair, a `WebVTT` segment, and a non-media file that must NEVER
/// be served.
fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    std::fs::write(dir.path().join("multiview.m3u8"), PLAYLIST_TEXT).unwrap();
    std::fs::write(dir.path().join("seg0.ts"), SEGMENT_BYTES).unwrap();
    std::fs::write(dir.path().join("part0.m4s"), SEGMENT_BYTES).unwrap();
    std::fs::write(dir.path().join("init.mp4"), SEGMENT_BYTES).unwrap();
    std::fs::write(dir.path().join("sub0.vtt"), "WEBVTT\n").unwrap();
    std::fs::write(dir.path().join("multiview.toml"), "secret = true\n").unwrap();
    dir
}

/// Drive one request through the router (in-process; the crate's
/// `tower::oneshot` HTTP test pattern).
async fn send(
    dir: &tempfile::TempDir,
    method: Method,
    path: &str,
    headers: &[(header::HeaderName, &str)],
) -> Response<axum::body::Body> {
    send_dir(&DirHandle(dir.path().to_path_buf()), method, path, headers).await
}

/// A served-directory handle for tests that need an explicit root (symlink /
/// large-file fixtures) rather than a [`tempfile::TempDir`].
struct DirHandle(std::path::PathBuf);

/// Drive one request against an explicit served root.
async fn send_dir(
    dir: &DirHandle,
    method: Method,
    path: &str,
    headers: &[(header::HeaderName, &str)],
) -> Response<axum::body::Body> {
    let router = hls_router(dir.0.clone());
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    let request = request.body(Body::empty()).expect("build request");
    router.oneshot(request).await.expect("infallible service")
}

/// A response header's value as an owned `String`, if present and UTF-8.
fn header_str(response: &Response<axum::body::Body>, name: header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
}

/// Collect a response body to bytes.
async fn body_bytes(response: Response<axum::body::Body>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec()
}

/// Preflight `OPTIONS` answers `204` with the full CORS allow set on BOTH the
/// playlist and segment routes.
#[tokio::test]
async fn preflight_options_returns_204_with_cors_allow_set() {
    let dir = fixture_dir();
    for path in ["/multiview.m3u8", "/seg0.ts", "/part0.m4s", "/init.mp4"] {
        let response = send(
            &dir,
            Method::OPTIONS,
            path,
            &[
                (header::ORIGIN, ORIGIN),
                (header::ACCESS_CONTROL_REQUEST_METHOD, "GET"),
            ],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "preflight on {path} must answer 204"
        );
        let headers = response.headers();
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some(ORIGIN),
            "preflight on {path} must reflect the Origin"
        );
        assert_eq!(
            headers.get(header::VARY).and_then(|v| v.to_str().ok()),
            Some("Origin"),
            "preflight on {path} must vary by Origin"
        );
        let methods = headers
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        for method in ["GET", "HEAD", "OPTIONS"] {
            assert!(
                methods.contains(method),
                "preflight on {path} must allow {method}, got {methods:?}"
            );
        }
        let allow_headers = headers
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        for name in ["Content-Type", "Range", "Accept-Encoding"] {
            assert!(
                allow_headers.contains(name),
                "preflight on {path} must allow header {name}, got {allow_headers:?}"
            );
        }
        assert!(
            headers.contains_key(header::ACCESS_CONTROL_MAX_AGE),
            "preflight on {path} must carry Access-Control-Max-Age"
        );
    }
}

/// A cross-origin playlist GET reflects the Origin (never `*`), varies by
/// Origin, exposes `Content-Length`/`Content-Range`, and carries the
/// ADR-0032 live-playlist Cache-Control tier + Content-Type.
#[tokio::test]
async fn get_playlist_with_origin_reflects_origin_with_vary_and_expose() {
    let dir = fixture_dir();
    let response = send(
        &dir,
        Method::GET,
        "/multiview.m3u8",
        &[(header::ORIGIN, ORIGIN)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN),
        "the response must reflect the request Origin, not a wildcard"
    );
    assert_eq!(
        headers.get(header::VARY).and_then(|v| v.to_str().ok()),
        Some("Origin")
    );
    let exposed = headers
        .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    for name in ["Content-Length", "Content-Range"] {
        assert!(
            exposed.contains(name),
            "must expose {name}, got {exposed:?}"
        );
    }
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("max-age=1, must-revalidate"),
        "live playlists carry the short-TTL Cache-Control tier (ADR-0032 §6)"
    );
    assert_eq!(body_bytes(response).await, PLAYLIST_TEXT.as_bytes());
}

/// A request WITHOUT an Origin header (a normal player) gets NO CORS headers —
/// but still varies by Origin so a shared cache never serves the
/// header-less variant to a cross-origin caller.
#[tokio::test]
async fn get_without_origin_carries_no_cors_headers() {
    let dir = fixture_dir();
    let response = send(&dir, Method::GET, "/seg0.ts", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    for name in [
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::ACCESS_CONTROL_ALLOW_METHODS,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        header::ACCESS_CONTROL_MAX_AGE,
    ] {
        assert!(
            !headers.contains_key(&name),
            "a no-Origin request must carry no {name} header"
        );
    }
    assert_eq!(
        headers.get(header::VARY).and_then(|v| v.to_str().ok()),
        Some("Origin"),
        "Vary: Origin protects shared caches even on no-Origin responses"
    );
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("video/mp2t")
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable"),
        "closed segments carry the immutable Cache-Control tier (ADR-0032 §6)"
    );
}

/// CORS must not break Range: a cross-origin Range GET on a segment serves
/// `206` with `Content-Range`, the exact byte span, `Accept-Ranges: bytes`,
/// the reflected Origin, and `Content-Range` exposed to the caller.
#[tokio::test]
async fn range_get_with_origin_serves_206_with_cors_and_content_range() {
    let dir = fixture_dir();
    let response = send(
        &dir,
        Method::GET,
        "/seg0.ts",
        &[(header::ORIGIN, ORIGIN), (header::RANGE, "bytes=2-5")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes 2-5/16")
    );
    assert_eq!(
        headers
            .get(header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("bytes")
    );
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN)
    );
    let exposed = headers
        .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        exposed.contains("Content-Range"),
        "Content-Range must be CORS-exposed so players can read it, got {exposed:?}"
    );
    assert_eq!(body_bytes(response).await, b"2345");
}

/// A syntactically-valid but unsatisfiable range answers `416` with the
/// `bytes */len` Content-Range form (still CORS-tagged for cross-origin
/// callers).
#[tokio::test]
async fn unsatisfiable_range_yields_416_with_total_length() {
    let dir = fixture_dir();
    let response = send(
        &dir,
        Method::GET,
        "/seg0.ts",
        &[(header::ORIGIN, ORIGIN), (header::RANGE, "bytes=99-")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes */16")
    );
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN)
    );
}

/// `HEAD` answers the same headers as `GET` — including the real
/// `Content-Length` — with an empty body (metadata only; no segment read).
#[tokio::test]
async fn head_serves_metadata_with_content_length_and_no_body() {
    let dir = fixture_dir();
    let response = send(&dir, Method::HEAD, "/seg0.ts", &[(header::ORIGIN, ORIGIN)]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("16")
    );
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN)
    );
    assert!(body_bytes(response).await.is_empty());
}

/// Only HLS media extensions are served: a sibling non-media file 404s, and a
/// path-traversal attempt can never escape the segment directory.
#[tokio::test]
async fn non_media_files_and_traversal_are_never_served() {
    let dir = fixture_dir();
    // The non-media sibling exists on disk but is not HLS delivery surface.
    let response = send(&dir, Method::GET, "/multiview.toml", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Traversal out of the root is rejected (encoded dot-dot segments).
    let response = send(&dir, Method::GET, "/%2e%2e/%2e%2e/etc/passwd", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A symlink **inside** the served root that points OUTSIDE it must never be
/// followed: the lexical pre-check passes (the request path has only Normal
/// components), but canonicalisation resolves the link's real target outside
/// the canonical root and the handler must 404 — no secret bytes leak.
///
/// Mirrors the reviewer's repro: a sibling `secret/` directory holding a
/// `.ts` "segment" with a sentinel body, plus (a) a directory symlink and
/// (b) a media-extension file symlink inside the served root, each pointing
/// into that sibling secret. Both must 404 with no secret content served.
#[cfg(unix)]
#[tokio::test]
async fn symlink_escaping_the_served_root_is_never_followed() {
    use std::os::unix::fs::symlink;

    const SECRET: &[u8] = b"TOP-SECRET-NOT-DELIVERY-SURFACE";

    // A parent tree with two siblings: `served/` (the HLS root) and `secret/`
    // (outside the root) holding a media-extension file we must never serve.
    let parent = tempfile::tempdir().expect("create temp dir");
    let served = parent.path().join("served");
    let secret = parent.path().join("secret");
    std::fs::create_dir(&served).unwrap();
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(served.join("ok.ts"), SEGMENT_BYTES).unwrap();
    std::fs::write(secret.join("leak.ts"), SECRET).unwrap();

    // (a) A directory symlink inside the root pointing at the sibling secret dir.
    symlink(&secret, served.join("escape")).unwrap();
    // (b) A file symlink (media extension) inside the root pointing at the secret.
    symlink(secret.join("leak.ts"), served.join("link.ts")).unwrap();

    let dir_handle = DirHandle(served);

    // Sanity: a real file inside the root still serves.
    let response = send_dir(&dir_handle, Method::GET, "/ok.ts", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, SEGMENT_BYTES);

    // The directory-symlink traversal must 404 and never leak secret bytes.
    let response = send_dir(&dir_handle, Method::GET, "/escape/leak.ts", &[]).await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a dir symlink escaping the served root must not be followed"
    );
    let body = body_bytes(response).await;
    assert_ne!(body, SECRET, "no secret bytes may be served");

    // The file-symlink escape must 404 and never leak secret bytes.
    let response = send_dir(&dir_handle, Method::GET, "/link.ts", &[]).await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a file symlink escaping the served root must not be followed"
    );
    let body = body_bytes(response).await;
    assert_ne!(body, SECRET, "no secret bytes may be served");
}

/// A large segment (well past any sane single read buffer) is served correctly
/// in full and by Range — the streaming body must reproduce the file
/// byte-for-byte. This is the behavioural guard that the streaming rewrite
/// (no whole-file `vec![0; len]` buffering) keeps bodies identical.
#[tokio::test]
async fn large_segment_streams_full_and_range_byte_for_byte() {
    // 5 MiB of deterministic, non-repeating-per-byte content so a truncated or
    // mis-seeked stream is detectable.
    let len: usize = 5 * 1024 * 1024;
    let mut bytes = vec![0_u8; len];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::try_from(i % 251).unwrap();
    }
    let dir = tempfile::tempdir().expect("create temp dir");
    std::fs::write(dir.path().join("big.ts"), &bytes).unwrap();
    let dir_handle = DirHandle(dir.path().to_path_buf());
    // Keep the TempDir alive for the duration of the requests.
    let _keep = dir;

    // Full GET reproduces the whole file.
    let response = send_dir(&dir_handle, Method::GET, "/big.ts", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_str(&response, header::CONTENT_LENGTH),
        Some(len.to_string())
    );
    assert_eq!(body_bytes(response).await, bytes);

    // A Range across an internal span streams exactly that span.
    let start = 1_000_000_usize;
    let end = 4_000_000_usize;
    let response = send_dir(
        &dir_handle,
        Method::GET,
        "/big.ts",
        &[(header::RANGE, &format!("bytes={start}-{end}"))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_str(&response, header::CONTENT_RANGE),
        Some(format!("bytes {start}-{end}/{len}"))
    );
    assert_eq!(body_bytes(response).await, bytes[start..=end].to_vec());
}

proptest! {
    /// `byte_range` never panics and always answers one of the three contract
    /// outcomes, with any satisfiable span fully inside the file.
    #[test]
    fn byte_range_outcomes_are_always_in_bounds(header in "\\PC*", len in 0_u64..1_048_576) {
        match byte_range(&header, len) {
            ByteRange::Span { start, end } => {
                prop_assert!(start <= end);
                prop_assert!(end < len);
            }
            ByteRange::Ignored | ByteRange::Unsatisfiable => {}
        }
    }

    /// A well-formed `bytes=a-b` span inside the file parses exactly, clamped
    /// to the final byte.
    #[test]
    fn byte_range_parses_bounded_spans(start in 0_u64..4096, span in 0_u64..4096, len in 1_u64..8192) {
        let end = start.saturating_add(span);
        let parsed = byte_range(&format!("bytes={start}-{end}"), len);
        if start < len {
            prop_assert_eq!(parsed, ByteRange::Span { start, end: end.min(len - 1) });
        } else {
            prop_assert_eq!(parsed, ByteRange::Unsatisfiable);
        }
    }

    /// A suffix range `bytes=-n` takes the last `min(n, len)` bytes.
    #[test]
    fn byte_range_parses_suffix_spans(n in 1_u64..8192, len in 1_u64..8192) {
        let parsed = byte_range(&format!("bytes=-{n}"), len);
        prop_assert_eq!(
            parsed,
            ByteRange::Span { start: len.saturating_sub(n), end: len - 1 }
        );
    }
}
