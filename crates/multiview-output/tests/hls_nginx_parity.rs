//! Slice 6 (issue #292): the engine's HLS delivery replaces nginx on mda001,
//! so it must answer like the nginx contract it replaces
//! (`deploy/hls-nginx.conf` + nginx's static-file module). One golden table (25 rows):
//! each row is a request and the exact status plus the headers nginx sends
//! for it (`Cache-Control`, `Content-Type`, `Accept-Ranges`, `Content-Range`,
//! `Content-Length`, CORS, `ETag`/`Last-Modified` and `304`, `If-Range`).
//!
//! nginx facts the table encodes (nginx 1.26, `ngx_http_static_module`,
//! `ngx_http_range_filter_module`, `ngx_http_not_modified_filter_module`):
//! * `ETag: "<mtime hex>-<size hex>"`, `Last-Modified` from the mtime;
//! * `If-None-Match` match -> `304` with the validators and `Cache-Control`
//!   (the `always` `add_header`), no body; `If-Modified-Since` equal -> `304`;
//! * a single range -> `206` + `Content-Range: bytes a-b/len`; an
//!   unsatisfiable one -> `416` + `Content-Range: bytes */len`;
//! * `If-Range` that does not match the current `ETag` -> the full `200`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::body::Body;
use axum::http::{header, HeaderName, Method, Request, StatusCode};
use http_body_util::BodyExt;
use multiview_output::hls::http::hls_router;
use tower::util::ServiceExt;

const SEGMENT: &[u8] = b"0123456789abcdef";
const PLAYLIST: &[u8] = b"#EXTM3U\n#EXT-X-VERSION:7\n";

struct Fixture {
    dir: tempfile::TempDir,
    seg_etag: String,
    seg_last_modified: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("live.m3u8"), PLAYLIST).unwrap();
    std::fs::write(dir.path().join("seg1.m4s"), SEGMENT).unwrap();
    std::fs::write(dir.path().join("seg1.ts"), SEGMENT).unwrap();
    std::fs::write(dir.path().join("init.mp4"), SEGMENT).unwrap();
    std::fs::write(dir.path().join("sub1.vtt"), b"WEBVTT\n").unwrap();
    let meta = std::fs::metadata(dir.path().join("seg1.m4s")).unwrap();
    let modified = meta.modified().unwrap();
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    Fixture {
        seg_etag: format!("\"{secs:x}-{:x}\"", SEGMENT.len()),
        seg_last_modified: httpdate::fmt_http_date(modified),
        dir,
    }
}

struct Row {
    name: &'static str,
    method: Method,
    path: &'static str,
    request: Vec<(HeaderName, String)>,
    status: StatusCode,
    /// Headers that must be present with exactly this value.
    expect: Vec<(HeaderName, String)>,
    /// Headers that must be absent.
    absent: Vec<HeaderName>,
    body: Option<&'static [u8]>,
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const PLAYLIST_CACHE: &str = "max-age=1, must-revalidate";

#[allow(clippy::too_many_lines)]
fn table(f: &Fixture) -> Vec<Row> {
    let s = |v: &str| v.to_owned();
    let etag = f.seg_etag.clone();
    let lm = f.seg_last_modified.clone();
    vec![
        Row {
            name: "segment GET",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![],
            status: StatusCode::OK,
            expect: vec![
                (header::CONTENT_TYPE, s("video/iso.segment")),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
                (header::ACCEPT_RANGES, s("bytes")),
                (header::CONTENT_LENGTH, s("16")),
                (header::ETAG, etag.clone()),
                (header::LAST_MODIFIED, lm.clone()),
                (header::VARY, s("Origin")),
            ],
            absent: vec![
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                header::CONTENT_RANGE,
                header::CONTENT_ENCODING,
            ],
            body: Some(SEGMENT),
        },
        Row {
            name: "ts segment type",
            method: Method::GET,
            path: "/seg1.ts",
            request: vec![],
            status: StatusCode::OK,
            expect: vec![
                (header::CONTENT_TYPE, s("video/mp2t")),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
            ],
            absent: vec![],
            body: Some(SEGMENT),
        },
        Row {
            name: "init type",
            method: Method::GET,
            path: "/init.mp4",
            request: vec![],
            status: StatusCode::OK,
            expect: vec![
                (header::CONTENT_TYPE, s("video/mp4")),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
            ],
            absent: vec![],
            body: None,
        },
        Row {
            name: "vtt type",
            method: Method::GET,
            path: "/sub1.vtt",
            request: vec![],
            status: StatusCode::OK,
            expect: vec![
                (header::CONTENT_TYPE, s("text/vtt")),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
            ],
            absent: vec![],
            body: None,
        },
        Row {
            name: "playlist GET",
            method: Method::GET,
            path: "/live.m3u8",
            request: vec![],
            status: StatusCode::OK,
            expect: vec![
                (header::CONTENT_TYPE, s("application/vnd.apple.mpegurl")),
                (header::CACHE_CONTROL, s(PLAYLIST_CACHE)),
                (header::CONTENT_LENGTH, PLAYLIST.len().to_string()),
            ],
            absent: vec![],
            body: Some(PLAYLIST),
        },
        Row {
            name: "segment HEAD",
            method: Method::HEAD,
            path: "/seg1.m4s",
            request: vec![],
            status: StatusCode::OK,
            expect: vec![
                (header::CONTENT_LENGTH, s("16")),
                (header::ETAG, etag.clone()),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
            ],
            absent: vec![],
            body: Some(b""),
        },
        Row {
            name: "closed range",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::RANGE, s("bytes=2-5"))],
            status: StatusCode::PARTIAL_CONTENT,
            expect: vec![
                (header::CONTENT_RANGE, s("bytes 2-5/16")),
                (header::CONTENT_LENGTH, s("4")),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
                (header::ETAG, etag.clone()),
            ],
            absent: vec![],
            body: Some(b"2345"),
        },
        Row {
            name: "suffix range",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::RANGE, s("bytes=-3"))],
            status: StatusCode::PARTIAL_CONTENT,
            expect: vec![(header::CONTENT_RANGE, s("bytes 13-15/16"))],
            absent: vec![],
            body: Some(b"def"),
        },
        Row {
            name: "open range clamps",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::RANGE, s("bytes=10-999"))],
            status: StatusCode::PARTIAL_CONTENT,
            expect: vec![(header::CONTENT_RANGE, s("bytes 10-15/16"))],
            absent: vec![],
            body: Some(b"abcdef"),
        },
        Row {
            name: "unsatisfiable range",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::RANGE, s("bytes=16-"))],
            status: StatusCode::RANGE_NOT_SATISFIABLE,
            expect: vec![(header::CONTENT_RANGE, s("bytes */16"))],
            absent: vec![],
            body: None,
        },
        Row {
            name: "multi-range served whole",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::RANGE, s("bytes=0-1,4-5"))],
            status: StatusCode::OK,
            expect: vec![(header::CONTENT_LENGTH, s("16"))],
            absent: vec![header::CONTENT_RANGE],
            body: Some(SEGMENT),
        },
        Row {
            name: "If-None-Match hit",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::IF_NONE_MATCH, etag.clone())],
            status: StatusCode::NOT_MODIFIED,
            expect: vec![
                (header::ETAG, etag.clone()),
                (header::LAST_MODIFIED, lm.clone()),
                (header::CACHE_CONTROL, s(IMMUTABLE)),
            ],
            absent: vec![header::CONTENT_RANGE],
            body: Some(b""),
        },
        Row {
            name: "If-None-Match list and weak form",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::IF_NONE_MATCH, format!("\"x\", W/{etag}"))],
            status: StatusCode::NOT_MODIFIED,
            expect: vec![],
            absent: vec![],
            body: Some(b""),
        },
        Row {
            name: "weak If-None-Match with Range serves the range",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![
                (header::RANGE, s("bytes=0-1")),
                (header::IF_NONE_MATCH, format!("W/{etag}")),
            ],
            status: StatusCode::PARTIAL_CONTENT,
            expect: vec![(header::CONTENT_RANGE, s("bytes 0-1/16"))],
            absent: vec![],
            body: Some(b"01"),
        },
        Row {
            name: "star If-None-Match with Range serves the range",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![
                (header::RANGE, s("bytes=2-3")),
                (header::IF_NONE_MATCH, s("*")),
            ],
            status: StatusCode::PARTIAL_CONTENT,
            expect: vec![(header::CONTENT_RANGE, s("bytes 2-3/16"))],
            absent: vec![],
            body: Some(b"23"),
        },
        Row {
            name: "strong If-None-Match with Range is 304",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![
                (header::RANGE, s("bytes=0-1")),
                (header::IF_NONE_MATCH, etag.clone()),
            ],
            status: StatusCode::NOT_MODIFIED,
            expect: vec![],
            absent: vec![header::CONTENT_RANGE],
            body: Some(b""),
        },
        Row {
            name: "If-None-Match miss",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::IF_NONE_MATCH, s("\"0-0\""))],
            status: StatusCode::OK,
            expect: vec![],
            absent: vec![],
            body: Some(SEGMENT),
        },
        Row {
            name: "If-Modified-Since equal",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::IF_MODIFIED_SINCE, lm.clone())],
            status: StatusCode::NOT_MODIFIED,
            expect: vec![(header::CACHE_CONTROL, s(IMMUTABLE))],
            absent: vec![],
            body: Some(b""),
        },
        Row {
            name: "If-Modified-Since older",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(
                header::IF_MODIFIED_SINCE,
                s("Thu, 01 Jan 1970 00:00:01 GMT"),
            )],
            status: StatusCode::OK,
            expect: vec![],
            absent: vec![],
            body: Some(SEGMENT),
        },
        Row {
            name: "If-Range current",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![
                (header::RANGE, s("bytes=0-1")),
                (header::IF_RANGE, etag.clone()),
            ],
            status: StatusCode::PARTIAL_CONTENT,
            expect: vec![(header::CONTENT_RANGE, s("bytes 0-1/16"))],
            absent: vec![],
            body: Some(b"01"),
        },
        Row {
            name: "If-Range stale",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![
                (header::RANGE, s("bytes=0-1")),
                (header::IF_RANGE, s("\"0-0\"")),
            ],
            status: StatusCode::OK,
            expect: vec![(header::CONTENT_LENGTH, s("16"))],
            absent: vec![header::CONTENT_RANGE],
            body: Some(SEGMENT),
        },
        Row {
            name: "cross-origin",
            method: Method::GET,
            path: "/seg1.m4s",
            request: vec![(header::ORIGIN, s("https://receiver.example"))],
            status: StatusCode::OK,
            expect: vec![
                (
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    s("https://receiver.example"),
                ),
                (
                    header::ACCESS_CONTROL_EXPOSE_HEADERS,
                    s("Content-Length, Content-Range"),
                ),
                (header::VARY, s("Origin")),
            ],
            absent: vec![],
            body: None,
        },
        Row {
            name: "preflight",
            method: Method::OPTIONS,
            path: "/seg1.m4s",
            request: vec![(header::ORIGIN, s("https://receiver.example"))],
            status: StatusCode::NO_CONTENT,
            expect: vec![
                (
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    s("GET, HEAD, OPTIONS"),
                ),
                (
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    s("Content-Type, Range, Accept-Encoding"),
                ),
                (header::ACCESS_CONTROL_MAX_AGE, s("86400")),
            ],
            absent: vec![],
            body: None,
        },
        Row {
            name: "non-media 404",
            method: Method::GET,
            path: "/conspect.toml",
            request: vec![],
            status: StatusCode::NOT_FOUND,
            expect: vec![],
            absent: vec![],
            body: None,
        },
        Row {
            name: "missing segment 404",
            method: Method::GET,
            path: "/seg9.m4s",
            request: vec![],
            status: StatusCode::NOT_FOUND,
            expect: vec![],
            absent: vec![],
            body: None,
        },
    ]
}

#[tokio::test]
async fn hls_answers_the_nginx_golden_table() {
    let f = fixture();
    let rows = table(&f);
    let count = rows.len();
    for row in rows {
        let mut request = Request::builder().method(row.method.clone()).uri(row.path);
        for (name, value) in &row.request {
            request = request.header(name, value);
        }
        let response = hls_router(f.dir.path().to_path_buf())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), row.status, "{}", row.name);
        for (name, value) in &row.expect {
            assert_eq!(
                response.headers().get(name).and_then(|v| v.to_str().ok()),
                Some(value.as_str()),
                "{}: {name}",
                row.name
            );
        }
        for name in &row.absent {
            assert!(
                response.headers().get(name).is_none(),
                "{}: {name} must be absent",
                row.name
            );
        }
        if let Some(body) = row.body {
            let got = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&got[..], body, "{}", row.name);
        }
    }
    assert_eq!(count, 25);
}

/// A segment larger than one body chunk streams byte for byte, in bounded
/// chunks (never one allocation of the whole file).
#[tokio::test]
async fn large_segment_streams_in_bounded_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..(5 * multiview_output::hls::http::BODY_CHUNK + 123))
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    std::fs::write(dir.path().join("big.m4s"), &data).unwrap();
    let response = hls_router(dir.path().to_path_buf())
        .oneshot(
            Request::builder()
                .uri("/big.m4s")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut got = Vec::new();
    let mut frames = 0;
    while let Some(frame) = body.frame().await {
        let chunk = frame.unwrap().into_data().unwrap();
        assert!(chunk.len() <= multiview_output::hls::http::BODY_CHUNK);
        got.extend_from_slice(&chunk);
        frames += 1;
    }
    assert_eq!(got, data);
    assert!(frames >= 6, "{frames} frames");
}
