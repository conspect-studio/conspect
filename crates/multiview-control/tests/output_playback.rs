//! End-to-end tests for `GET /api/v1/outputs/{id}/playback` (ADR-0101 — "a
//! link to copy the actual URL to paste into a tool like VLC"): per-object
//! auth, SRT listener/caller/streamid/encrypted shapes, an IPv6 bind, the HLS
//! mount, and the `control.public_host` override vs the request `Host`
//! header fallback. Driven through the real router, mirroring `outputs.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, HeaderValue, StatusCode};
use serde_json::json;
use support::{
    body_json, get, harness, harness_with, post_json, send, ADMIN_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

fn get_with_host(path: &str, token: &str, host: &str) -> axum::http::Request<axum::body::Body> {
    let mut request = get(path, token);
    request
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_str(host).unwrap());
    request
}

/// An SRT listener output, with `public_host` configured, yields a viewer
/// `srt://…?mode=caller` URL at the public host.
#[tokio::test]
async fn srt_listener_yields_caller_url_at_public_host() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let output = json!({
        "name": "SRT listener",
        "body": {
            "kind": "srt",
            "url": "srt://[::]:9000",
            "codec": "h264",
            "srt": { "mode": "listener", "latency_ms": 120 }
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/srt-out", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get("/api/v1/outputs/srt-out/playback", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let urls = body["urls"].as_array().unwrap();
    assert_eq!(urls.len(), 1);
    assert_eq!(
        urls[0]["url"],
        "srt://viewer.example:9000?mode=caller&latency=120"
    );
}

/// An SRT caller (push) output has no viewer URL; the note redacts the
/// destination without leaking any query value.
#[tokio::test]
async fn srt_caller_push_has_no_viewer_url() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let output = json!({
        "name": "SRT push",
        "body": {
            "kind": "srt",
            "url": "srt://dest.example:9001?streamid=secret-key",
            "codec": "h264"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/srt-push", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get("/api/v1/outputs/srt-push/playback", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let urls = body["urls"].as_array().unwrap();
    assert!(urls[0].get("url").is_none());
    let notes = urls[0]["notes"].as_array().unwrap();
    assert!(notes[0].as_str().unwrap().contains("dest.example:9001"));
    // The query (which can carry a secret-ish value) must never leak.
    assert!(!notes[0].as_str().unwrap().contains("secret-key"));
}

/// A `streamid` on a listener is appended, percent-encoded.
#[tokio::test]
async fn srt_listener_streamid_is_percent_encoded() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let output = json!({
        "name": "SRT listener with stream id",
        "body": {
            "kind": "srt",
            "url": "srt://[::]:9000",
            "codec": "h264",
            "srt": { "mode": "listener", "stream_id": "#!::u=viewer1" }
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/srt-sid", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get("/api/v1/outputs/srt-sid/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    let url = body["urls"][0]["url"].as_str().unwrap();
    assert!(url.contains("streamid=%23%21%3A%3Au%3Dviewer1"), "{url}");
}

/// An encrypted SRT listener notes that a passphrase is required, and NEVER
/// returns the `secret_ref` or any resolved secret.
#[tokio::test]
async fn srt_listener_encrypted_notes_passphrase_required_never_leaks_secret_ref() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let output = json!({
        "name": "SRT listener encrypted",
        "body": {
            "kind": "srt",
            "url": "srt://[::]:9000",
            "codec": "h264",
            "srt": {
                "mode": "listener",
                "encryption": {
                    "key_length": "aes128",
                    "passphrase_ref": "op://Servers/feed/srt-passphrase"
                }
            }
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/srt-enc", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get("/api/v1/outputs/srt-enc/playback", ADMIN_TOKEN),
    )
    .await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(raw.contains("passphrase"), "{raw}");
    assert!(!raw.contains("op://Servers"), "secret_ref leaked: {raw}");
}

/// An IPv6-bracketed SRT listener bind resolves against an IPv4-literal
/// `public_host`.
#[tokio::test]
async fn srt_ipv6_bind_resolves_via_ipv4_public_host() {
    let h = harness_with(|state| state.with_public_host(Some("203.0.113.5".to_owned())));
    let output = json!({
        "name": "SRT listener ipv6 bind",
        "body": {
            "kind": "srt",
            "url": "srt://[2001:db8::10]:9000",
            "codec": "h264",
            "srt": { "mode": "listener" }
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/srt-v6", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get("/api/v1/outputs/srt-v6/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(body["urls"][0]["url"], "srt://203.0.113.5:9000?mode=caller");
}

/// An HLS output's playback URL is the absolute `/hls/{mount}` playlist URL,
/// mount-derived the same way the DEV-D1 router derives it.
#[tokio::test]
async fn hls_output_yields_absolute_playlist_url() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let output = json!({
        "name": "Program HLS",
        "body": {
            "kind": "hls",
            "path": "/var/lib/multiview/hls/program/multiview.m3u8",
            "codec": "h264"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/program", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get("/api/v1/outputs/program/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(
        body["urls"][0]["url"],
        "http://viewer.example/hls/program/multiview.m3u8"
    );
}

/// `control.public_host` wins over the request `Host` header.
#[tokio::test]
async fn configured_public_host_wins_over_host_header() {
    let h = harness_with(|state| state.with_public_host(Some("configured.example".to_owned())));
    let output = json!({
        "name": "Program HLS",
        "body": {
            "kind": "hls",
            "path": "/var/lib/multiview/hls/program/multiview.m3u8",
            "codec": "h264"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/program", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get_with_host(
            "/api/v1/outputs/program/playback",
            ADMIN_TOKEN,
            "header-host.example:8080",
        ),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(
        body["urls"][0]["url"],
        "http://configured.example/hls/program/multiview.m3u8"
    );
}

/// Without `control.public_host`, the request `Host` header is the fallback.
#[tokio::test]
async fn host_header_is_the_fallback_when_public_host_is_unset() {
    let h = harness();
    let output = json!({
        "name": "Program HLS",
        "body": {
            "kind": "hls",
            "path": "/var/lib/multiview/hls/program/multiview.m3u8",
            "codec": "h264"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/program", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        get_with_host(
            "/api/v1/outputs/program/playback",
            ADMIN_TOKEN,
            "header-host.example",
        ),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(
        body["urls"][0]["url"],
        "http://header-host.example/hls/program/multiview.m3u8"
    );
}

/// With neither a configured `public_host` nor a `Host` header, the route
/// returns an honest note instead of a guessed URL.
#[tokio::test]
async fn no_public_host_and_no_host_header_yields_a_note_not_a_guess() {
    let h = harness();
    let output = json!({
        "name": "Program HLS",
        "body": {
            "kind": "hls",
            "path": "/var/lib/multiview/hls/program/multiview.m3u8",
            "codec": "h264"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/program", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // `support::get` does not set a Host header; axum's default extraction
    // without one yields no header value to parse.
    let resp = send(
        &h.router,
        get("/api/v1/outputs/program/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert!(body["urls"][0].get("url").is_none());
    assert!(body["urls"][0]["notes"][0]
        .as_str()
        .unwrap()
        .contains("no public host"));
}

/// RBAC/BOLA: a viewer (read role) may read the playback endpoint; a
/// principal scoped to a different output id is denied this output's.
#[tokio::test]
async fn playback_requires_read_role_and_is_per_object_scoped() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let output = json!({
        "name": "Program HLS",
        "body": {
            "kind": "hls",
            "path": "/var/lib/multiview/hls/program/multiview.m3u8",
            "codec": "h264"
        }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/outputs/program", ADMIN_TOKEN, &output),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A viewer (read role, unscoped) can read it.
    let resp = send(
        &h.router,
        get("/api/v1/outputs/program/playback", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // `SCOPED_TOKEN` is scoped to object id `scoped-layout` only (BOLA,
    // SEC-08): it must be denied this DIFFERENT output id.
    let resp = send(
        &h.router,
        get("/api/v1/outputs/program/playback", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A nonexistent output id is `404`.
#[tokio::test]
async fn playback_404s_for_an_unknown_output() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/outputs/nope/playback", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A RIST listener bind (`[::]`) yields a plain viewer URL (no credentials to
/// redact); a RIST push destination yields a redacted-destination note.
#[tokio::test]
async fn rist_listener_vs_push() {
    let h = harness_with(|state| state.with_public_host(Some("viewer.example".to_owned())));
    let listener = json!({
        "name": "RIST listener",
        "body": { "kind": "rist", "url": "rist://[::]:5000", "codec": "h264" }
    });
    let push = json!({
        "name": "RIST push",
        "body": { "kind": "rist", "url": "rist://remote.example:5000", "codec": "h264" }
    });
    send(
        &h.router,
        post_json("/api/v1/outputs/rist-l", ADMIN_TOKEN, &listener),
    )
    .await;
    send(
        &h.router,
        post_json("/api/v1/outputs/rist-p", ADMIN_TOKEN, &push),
    )
    .await;

    let resp = send(
        &h.router,
        get("/api/v1/outputs/rist-l/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(body["urls"][0]["url"], "rist://viewer.example:5000");

    let resp = send(
        &h.router,
        get("/api/v1/outputs/rist-p/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert!(body["urls"][0].get("url").is_none());
    assert!(body["urls"][0]["notes"][0]
        .as_str()
        .unwrap()
        .contains("remote.example"));
}

/// An RTMP (always-push) output has no viewer URL.
#[tokio::test]
async fn rtmp_push_has_no_viewer_url() {
    let h = harness();
    let output = json!({
        "name": "RTMP push",
        "body": { "kind": "rtmp", "url": "rtmp://ingest.example/live/key", "codec": "h264" }
    });
    send(
        &h.router,
        post_json("/api/v1/outputs/rtmp-a", ADMIN_TOKEN, &output),
    )
    .await;

    let resp = send(
        &h.router,
        get("/api/v1/outputs/rtmp-a/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert!(body["urls"][0].get("url").is_none());
}

/// A `webrtc` output yields the relative WHEP play URL.
#[tokio::test]
async fn webrtc_output_yields_whep_url() {
    let h = harness();
    let output = json!({
        "name": "Program WebRTC",
        "body": { "kind": "webrtc", "id": "pgm-webrtc", "label": "Program WebRTC" }
    });
    send(
        &h.router,
        post_json("/api/v1/outputs/pgm-webrtc", ADMIN_TOKEN, &output),
    )
    .await;

    let resp = send(
        &h.router,
        get("/api/v1/outputs/pgm-webrtc/playback", ADMIN_TOKEN),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(body["urls"][0]["url"], "/api/v1/whep/pgm-webrtc");
}
