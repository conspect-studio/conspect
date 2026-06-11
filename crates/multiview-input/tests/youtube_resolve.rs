//! Fixture tests for the pure `YouTube` **resolver core** (ADR-0015 phase P0).
//!
//! These run only with `--features youtube`. They drive the *pure* parse layer —
//! [`parse_info_dict`] over recorded `yt-dlp -J` JSON and [`parse_expire`] over a
//! resolved `*.googlevideo.com` URL — so the manifest-extraction / live-status /
//! expiry contract is pinned with **no network and no subprocess**. The spawn
//! wrapper is a thin separate fn; the correctness load lives in these pure fns.
#![cfg(feature = "youtube")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use multiview_input::youtube::{
    parse_expire, parse_info_dict, probe_version, LiveStatus, ResolverConfig, YoutubeError,
};

/// A trimmed but representative `yt-dlp -J` info-dict for a **live** stream: a
/// top-level `live_status`/`is_live`, plus a `formats` array carrying an HLS
/// master (`protocol == "m3u8_native"`, with a `manifest_url`) alongside a
/// non-HLS progressive format that must be ignored. The googlevideo manifest URL
/// carries the `expire` Unix-timestamp query param.
const LIVE_INFO_JSON: &str = r#"{
  "id": "abcdEFGH123",
  "title": "Example Live Stream",
  "is_live": true,
  "live_status": "is_live",
  "formats": [
    {
      "format_id": "233",
      "url": "https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1893456000/file/index.m3u8",
      "manifest_url": "https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1893456000/file/index.m3u8",
      "protocol": "m3u8_native",
      "ext": "mp4",
      "vcodec": "avc1.4d401f",
      "acodec": "mp4a.40.2",
      "height": 720,
      "fps": 30,
      "tbr": 2500.0
    },
    {
      "format_id": "18",
      "url": "https://rr1---sn-abc.googlevideo.com/videoplayback?expire=1893456000&id=xyz",
      "protocol": "https",
      "ext": "mp4",
      "vcodec": "avc1.42001E",
      "acodec": "mp4a.40.2",
      "height": 360,
      "fps": 30
    }
  ]
}"#;

#[test]
fn parses_live_hls_master_from_info_dict() {
    let resolved = parse_info_dict(LIVE_INFO_JSON).expect("live info-dict must resolve");

    // The HLS master is the `manifest_url` of the `m3u8_native` format — NOT its
    // `url` (the variant playlist) and NOT the progressive `https` format.
    assert_eq!(
        resolved.manifest_url,
        "https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1893456000/file/index.m3u8"
    );
    assert_eq!(resolved.live_status, LiveStatus::Live);
    // The `expire` query param is parsed to a Unix-timestamp deadline.
    assert_eq!(resolved.expire_unix, Some(1_893_456_000));
}

/// An `is_upcoming` (scheduled, not-yet-live) stream. yt-dlp still returns an
/// info-dict, but with no playable live HLS master — classification must report
/// `Upcoming` and extraction must fail with a typed `NotLive`, never a panic.
const UPCOMING_INFO_JSON: &str = r#"{
  "id": "upcoming1",
  "title": "Scheduled Premiere",
  "is_live": false,
  "live_status": "is_upcoming",
  "release_timestamp": 1893456000,
  "formats": []
}"#;

#[test]
fn classifies_upcoming_and_refuses_extraction() {
    let err = parse_info_dict(UPCOMING_INFO_JSON).expect_err("upcoming has no live master");
    match err {
        YoutubeError::NotLive(status) => assert_eq!(status, LiveStatus::Upcoming),
        other => panic!("expected NotLive(Upcoming), got {other:?}"),
    }
}

/// A `post_live` DVR window (the broadcast just ended but a re-watchable buffer
/// remains). It is classified, but is not a *live* tile, so extraction refuses.
const POST_LIVE_INFO_JSON: &str = r#"{
  "id": "postlive1",
  "title": "Just Ended",
  "is_live": false,
  "live_status": "post_live",
  "formats": [
    {
      "format_id": "233",
      "manifest_url": "https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1893456000/file/index.m3u8",
      "protocol": "m3u8_native",
      "ext": "mp4"
    }
  ]
}"#;

#[test]
fn classifies_post_live_dvr() {
    let err = parse_info_dict(POST_LIVE_INFO_JSON).expect_err("post_live is not a live tile");
    match err {
        YoutubeError::NotLive(status) => assert_eq!(status, LiveStatus::PostLive),
        other => panic!("expected NotLive(PostLive), got {other:?}"),
    }
}

#[test]
fn live_without_hls_master_reports_no_hls() {
    // `is_live` but the formats carry only a progressive (`https`) format — no
    // `m3u8_native` master to feed libav. This must be a typed error, not a panic.
    const LIVE_NO_HLS: &str = r#"{
      "id": "nohls1",
      "live_status": "is_live",
      "is_live": true,
      "formats": [
        { "format_id": "18", "url": "https://x.googlevideo.com/videoplayback", "protocol": "https", "ext": "mp4" }
      ]
    }"#;
    let err = parse_info_dict(LIVE_NO_HLS).expect_err("no hls master must error");
    assert!(matches!(err, YoutubeError::NoHlsMaster));
}

#[test]
fn rejects_malformed_json() {
    let err = parse_info_dict("{ this is not json").expect_err("malformed json must error");
    assert!(matches!(err, YoutubeError::Json(_)));
}

#[test]
fn parses_expire_from_query_param() {
    // googlevideo URLs carry `expire` either as a `?expire=...` query value or as
    // a `/expire/.../` path segment; both forms must resolve to the same deadline.
    let q =
        "https://rr1---sn-abc.googlevideo.com/videoplayback?id=xyz&expire=1893456000&ip=1.2.3.4";
    assert_eq!(parse_expire(q), Some(1_893_456_000));

    let p =
        "https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1893456000/file/index.m3u8";
    assert_eq!(parse_expire(p), Some(1_893_456_000));

    // No expire present → None (the caller falls back to the TTL upper-bound).
    assert_eq!(parse_expire("https://example.com/index.m3u8"), None);
    // Non-numeric expire → None, never a panic.
    assert_eq!(parse_expire("https://x/?expire=soon"), None);
}

#[tokio::test]
async fn capability_probe_reports_unavailable_when_binary_absent() {
    // Point the resolver at a binary that does not exist: the probe must report
    // `Unavailable` cleanly (mirroring the NDI capability model) — never a panic,
    // and never a different error kind. No network, no real `yt-dlp` involved.
    let config = ResolverConfig::new(
        "/nonexistent/multiview-yt-dlp-test-binary",
        Duration::from_secs(5),
    );
    let err = probe_version(&config)
        .await
        .expect_err("a missing binary must not be reported as available");
    assert!(
        matches!(err, YoutubeError::Unavailable(_)),
        "expected Unavailable, got {err:?}"
    );
}

/// Property: the pure parser never panics on arbitrary bytes — any input either
/// resolves or returns a typed [`YoutubeError`] (ADR-0015 robustness).
mod prop {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_info_dict_never_panics(s in ".{0,512}") {
            // The result is discarded; the assertion is that the call returns at
            // all (no panic / no unwind) on arbitrary input.
            let _ = parse_info_dict(&s);
        }

        #[test]
        fn parse_expire_never_panics(s in ".{0,512}") {
            let _ = parse_expire(&s);
        }
    }
}
