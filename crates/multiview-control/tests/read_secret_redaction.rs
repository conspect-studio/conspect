//! SEC-03 (CWE-522 / OWASP API3:2023 — broken object *property*-level authz):
//! the per-resource READ paths and the config-revision READ paths must NOT
//! disclose an inline cleartext secret to a non-admin principal.
//!
//! `GET /api/v1/sources/{id}` + `GET /api/v1/outputs/{id}` (and their `list`
//! siblings), and `GET /api/v1/config/{target}` history + `/rev/{n}`, returned
//! the stored document VERBATIM — leaking a source's inline WHIP bearer `token`,
//! an output's `whip_push` bearer `token`, and a config revision's inline secret
//! to any [`Action::Read`] principal. A JWT/NMOS `read` grant maps to
//! [`Role::Viewer`]; that Viewer could harvest a source's WHIP token and then
//! WHIP-inject into the live program (privilege escalation). The config EXPORT
//! path already redacts these (`redact_config_for_export`); the READ paths did
//! not.
//!
//! The fix masks every inline secret on the read path for a non-admin principal
//! with the same `<redacted>` sentinel the export uses; an unscoped admin still
//! reads secrets verbatim (admin-reads-own-secret is not an escalation). The
//! store keeps the real secret — only the response VIEW is masked — so the
//! engine, a `resume` reload, and the admin still see the true value.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{
    body_bytes, body_json, get, harness, post_json, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    VIEWER_TOKEN,
};

/// Distinctive plaintext secret values that must never reach a non-admin reader.
const SOURCE_TOKEN: &str = "whip-src-bearer-SUPERSECRET-abc123";
const OUTPUT_TOKEN: &str = "whip-push-bearer-SUPERSECRET-def456";
const REVISION_SECRET: &str = "config-rev-inline-SUPERSECRET-ghi789";
/// The in-place placeholder a redacted secret is replaced with (mirrors
/// `support_bundle::EXPORT_REDACTED_SENTINEL`).
const SENTINEL: &str = "<redacted>";

/// Seed a WHIP-ingest source carrying an inline plaintext bearer `token`. The
/// token authorizes publishing INTO that source, so a reader who harvests it can
/// inject video into the live program — exactly the escalation this guards.
async fn seed_whip_source(h: &support::Harness) {
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/whip-cam",
            OPERATOR_TOKEN,
            &json!({
                "name": "WHIP cam",
                "body": { "id": "whip-cam", "kind": "webrtc", "token": SOURCE_TOKEN }
            }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the whip source seed must land"
    );
}

/// Seed a WHIP-push output carrying an inline plaintext bearer `token` (RFC 6750,
/// `Output::WhipPush`). The transport URL carries no credential, so the export
/// redactor (and this read-path redactor) keeps the URL but masks the token.
async fn seed_whip_push_output(h: &support::Harness) {
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/push-1",
            OPERATOR_TOKEN,
            &json!({
                "name": "WHIP push",
                "body": {
                    "kind": "whip_push",
                    "url": "https://[2001:db8::15]:8443/whip/pgm1",
                    "token": OUTPUT_TOKEN
                }
            }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the whip_push output seed must land"
    );
}

// ── Sources ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn viewer_get_source_redacts_inline_token_admin_sees_it() {
    let h = harness();
    seed_whip_source(&h).await;

    // A non-admin (Viewer) must NOT receive the plaintext bearer token.
    let resp = send(&h.router, get("/api/v1/sources/whip-cam", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        out["body"]["token"], SENTINEL,
        "a non-admin read must mask the inline WHIP token with the sentinel: {out}"
    );
    assert!(
        !text.contains(SOURCE_TOKEN),
        "the plaintext WHIP source token leaked to a Viewer:\n{text}"
    );

    // An unscoped admin still reads the secret verbatim (operational parity).
    let resp = send(&h.router, get("/api/v1/sources/whip-cam", ADMIN_TOKEN)).await;
    let out = body_json(resp).await;
    assert_eq!(
        out["body"]["token"], SOURCE_TOKEN,
        "an unscoped admin may read the secret (admin-reads-own-secret): {out}"
    );
}

#[tokio::test]
async fn viewer_list_sources_redacts_inline_token_admin_sees_it() {
    let h = harness();
    seed_whip_source(&h).await;

    let resp = send(&h.router, get("/api/v1/sources", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "whip-cam")
        .expect("the seeded source is listed");
    assert_eq!(
        row["body"]["token"], SENTINEL,
        "the list view must redact the inline token for a non-admin: {row}"
    );
    assert!(
        !text.contains(SOURCE_TOKEN),
        "the plaintext WHIP source token leaked in list_sources:\n{text}"
    );

    let resp = send(&h.router, get("/api/v1/sources", ADMIN_TOKEN)).await;
    let list = body_json(resp).await;
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "whip-cam")
        .unwrap();
    assert_eq!(
        row["body"]["token"], SOURCE_TOKEN,
        "an unscoped admin list still sees the secret: {row}"
    );
}

// ── Outputs ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn viewer_get_output_redacts_inline_token_admin_sees_it() {
    let h = harness();
    seed_whip_push_output(&h).await;

    let resp = send(&h.router, get("/api/v1/outputs/push-1", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    let out: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        out["body"]["token"], SENTINEL,
        "a non-admin read must mask the inline whip_push token: {out}"
    );
    // The non-secret transport URL is preserved (parity with config export).
    assert_eq!(
        out["body"]["url"], "https://[2001:db8::15]:8443/whip/pgm1",
        "the non-credential transport URL survives redaction: {out}"
    );
    assert!(
        !text.contains(OUTPUT_TOKEN),
        "the plaintext whip_push token leaked to a Viewer:\n{text}"
    );

    let resp = send(&h.router, get("/api/v1/outputs/push-1", ADMIN_TOKEN)).await;
    let out = body_json(resp).await;
    assert_eq!(
        out["body"]["token"], OUTPUT_TOKEN,
        "an unscoped admin may read the output secret: {out}"
    );
}

#[tokio::test]
async fn viewer_list_outputs_redacts_inline_token_admin_sees_it() {
    let h = harness();
    seed_whip_push_output(&h).await;

    let resp = send(&h.router, get("/api/v1/outputs", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "push-1")
        .expect("the seeded output is listed");
    assert_eq!(
        row["body"]["token"], SENTINEL,
        "the list view must redact the inline whip_push token for a non-admin: {row}"
    );
    assert!(
        !text.contains(OUTPUT_TOKEN),
        "the plaintext whip_push token leaked in list_outputs:\n{text}"
    );

    let resp = send(&h.router, get("/api/v1/outputs", ADMIN_TOKEN)).await;
    let list = body_json(resp).await;
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "push-1")
        .unwrap();
    assert_eq!(
        row["body"]["token"], OUTPUT_TOKEN,
        "an unscoped admin list still sees the output secret: {row}"
    );
}

// ── Config revisions ─────────────────────────────────────────────────────────

/// Commit a config revision whose document carries an inline secret (Write
/// role), then read it back: both the history list and the single-revision read
/// must mask the secret for a non-admin, and an admin still sees it.
#[tokio::test]
async fn non_admin_config_revision_reads_redact_inline_secret_admin_sees_it() {
    let h = harness();
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/pgm-cfg",
            OPERATOR_TOKEN,
            None,
            &json!({
                "document": { "token": REVISION_SECRET, "note": "keep-me" },
                "message": "seed a revision carrying an inline secret"
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "the commit must land");

    // History (list): the document's inline secret is masked for a Viewer.
    let resp = send(&h.router, get("/api/v1/config/pgm-cfg", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    let history: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        history[0]["document"]["token"], SENTINEL,
        "the revision history must redact the inline secret for a non-admin: {history}"
    );
    assert_eq!(
        history[0]["document"]["note"], "keep-me",
        "a non-secret field survives redaction: {history}"
    );
    assert!(
        !text.contains(REVISION_SECRET),
        "the config-revision secret leaked in the history read:\n{text}"
    );

    // Single revision: masked for a Viewer.
    let resp = send(&h.router, get("/api/v1/config/pgm-cfg/rev/1", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rev = body_json(resp).await;
    assert_eq!(
        rev["document"]["token"], SENTINEL,
        "the single-revision read must redact the inline secret for a non-admin: {rev}"
    );

    // An admin reads the revision secret verbatim (the store kept the real value).
    let resp = send(&h.router, get("/api/v1/config/pgm-cfg/rev/1", ADMIN_TOKEN)).await;
    let rev = body_json(resp).await;
    assert_eq!(
        rev["document"]["token"], REVISION_SECRET,
        "an unscoped admin reads the revision secret verbatim: {rev}"
    );
}

/// Bypass closure (rule 6): redacting `get_revision`/history is trivially
/// bypassable if `POST /config/{target}/rollback` still echoes a prior
/// revision's document verbatim — a non-admin Write principal could roll back to
/// any historical revision and read its secret in the response. The rollback
/// RESPONSE view must be masked for a non-admin too; the appended revision still
/// stores the real secret (an admin read confirms it), so the rollback is
/// functionally intact — only the non-admin's echo is masked.
#[tokio::test]
async fn non_admin_rollback_response_redacts_inline_secret() {
    let h = harness();
    // Revision 1 carries the secret; revision 2 replaces it (so a rollback to 1
    // is a real state change, not a no-op).
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/rb-cfg",
            OPERATOR_TOKEN,
            None,
            &json!({ "document": { "token": REVISION_SECRET, "note": "v1" }, "message": "v1" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/rb-cfg",
            OPERATOR_TOKEN,
            None,
            &json!({ "document": { "note": "v2" }, "message": "v2" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A non-admin Operator rolls back to revision 1: the echoed document (which
    // it did not author) must have its inline secret masked.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/config/rb-cfg/rollback",
            OPERATOR_TOKEN,
            &json!({ "to": 1 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body_bytes(resp).await;
    let text = String::from_utf8(bytes.clone()).unwrap();
    let rolled: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        rolled["document"]["token"], SENTINEL,
        "the rollback response must not echo an inline secret to a non-admin: {rolled}"
    );
    assert!(
        !text.contains(REVISION_SECRET),
        "the config-revision secret leaked in the rollback response:\n{text}"
    );

    // The store kept the real secret: an admin read of the original revision 1
    // still returns the cleartext (rollback masked only the non-admin echo).
    let resp = send(&h.router, get("/api/v1/config/rb-cfg/rev/1", ADMIN_TOKEN)).await;
    let rev = body_json(resp).await;
    assert_eq!(
        rev["document"]["token"], REVISION_SECRET,
        "the rollback masked only the response view; the stored secret is intact: {rev}"
    );
}

// ── SRT-03-R2: credential-bearing URLs in the RESOURCE BODY (outputs + sources) ──
//
// Round-2 SRT review (review-srt-round2.md): `GET /outputs`, `GET /outputs/{id}`,
// `GET /sources` and `GET /sources/{id}` return the stored resource body through
// `redact_inline_secrets_for_read`, which masked secret-NAMED keys but, before
// this fix, left every `url`/`peers` value untouched — so a credential embedded
// IN the url (an rtmp stream key, an SRT passphrase/streamid, a RIST secret or
// userinfo, or an HLS/TS query-string access token) reached a read-role viewer
// even though the engine LINK snapshot (`/outputs/{id}/link`, SRT-03 round 1) was
// already masked. These tests store the raw credential-bearing URL in the
// RESOURCE BODY itself (not just the link snapshot) and read it back as a
// read-role (Viewer) principal for every named transport/kind, asserting the
// secret never appears anywhere in the response — including the list response.

/// Every (field, raw-url-with-a-credential) case the brief names: rtmp (full and
/// single-segment), srt passphrase+streamid, rist secret+userinfo, and a query-
/// string access token on hls/ts. The literal plaintext secret for each case is
/// asserted absent from the full response body text.
fn srt03_r2_url_cases() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "rtmp-multi-segment",
            "rtmp://live.example/app/sk_live_SECRET_MULTI",
        ),
        (
            "rtmp-single-segment",
            "rtmp://live.example/sk_live_SECRET_SINGLE",
        ),
        (
            "srt-passphrase-streamid",
            "srt://[2001:db8::1]:9000?mode=caller&passphrase=SECRET_SRT_PASS&streamid=SECRET_SRT_SID",
        ),
        (
            "rist-secret-userinfo",
            "rist://ristuser:SECRET_RIST_USERINFO@[2001:db8::2]:8000?secret=SECRET_RIST_PSK",
        ),
        (
            "hls-query-token",
            "https://cdn.example/live/master.m3u8?token=SECRET_HLS_TOKEN",
        ),
        (
            "ts-query-token",
            "https://cdn.example/live/stream.ts?auth=SECRET_TS_TOKEN",
        ),
        (
            "rtsp-userinfo",
            "rtsp://camuser:SECRET_RTSP_PASS@[2001:db8::3]:554/main",
        ),
    ]
}

/// One secret marker per case, used only to assert absence (never asserted
/// present — the whole point is that it must never appear).
fn secret_marker_for(case: &str) -> &'static str {
    match case {
        "rtmp-multi-segment" => "SECRET_MULTI",
        "rtmp-single-segment" => "SECRET_SINGLE",
        "srt-passphrase-streamid" => "SECRET_SRT",
        "rist-secret-userinfo" => "SECRET_RIST",
        "hls-query-token" => "SECRET_HLS_TOKEN",
        "ts-query-token" => "SECRET_TS_TOKEN",
        "rtsp-userinfo" => "SECRET_RTSP_PASS",
        other => panic!("unlisted case: {other}"),
    }
}

#[tokio::test]
async fn viewer_get_and_list_outputs_never_disclose_a_credential_in_the_stored_url() {
    for (case, raw_url) in srt03_r2_url_cases() {
        // rtsp is not a valid output transport; this case is sources-only.
        if case == "rtsp-userinfo" {
            continue;
        }
        let h = harness();
        let id = format!("out-{case}");
        let kind = if case.starts_with("rtmp") {
            "rtmp"
        } else if case.starts_with("srt") {
            "srt"
        } else if case.starts_with("rist") {
            "rist"
        } else {
            // hls/ts query-token cases: exercised on an rtmp-shaped output body
            // is wrong (rtmp requires codec + rtmp url shape); these two cases
            // are output-irrelevant (HLS/TS are pull-only Source kinds, not
            // documented Output push kinds), so only emit them for sources.
            continue;
        };
        let resp = send(
            &h.router,
            post_json(
                &format!("/api/v1/outputs/{id}"),
                OPERATOR_TOKEN,
                &json!({
                    "name": case,
                    "body": { "kind": kind, "id": id, "url": raw_url, "codec": "h264" }
                }),
            ),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "{case}: output seed must land: {:?}",
            body_json(resp).await
        );

        let marker = secret_marker_for(case);

        // GET /outputs/{id}
        let resp = send(
            &h.router,
            get(&format!("/api/v1/outputs/{id}"), VIEWER_TOKEN),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains(marker),
            "{case}: the credential leaked in GET /outputs/{{id}}: {text}"
        );

        // GET /outputs (list)
        let resp = send(&h.router, get("/api/v1/outputs", VIEWER_TOKEN)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains(marker),
            "{case}: the credential leaked in GET /outputs (list): {text}"
        );

        // An admin still sees the raw credential (operational parity; the
        // stored document itself is untouched).
        let resp = send(
            &h.router,
            get(&format!("/api/v1/outputs/{id}"), ADMIN_TOKEN),
        )
        .await;
        let out = body_json(resp).await;
        assert_eq!(
            out["body"]["url"], raw_url,
            "{case}: an admin reads the stored URL verbatim: {out}"
        );
    }
}

#[tokio::test]
async fn viewer_get_and_list_sources_never_disclose_a_credential_in_the_stored_url() {
    for (case, raw_url) in srt03_r2_url_cases() {
        let h = harness();
        let id = format!("src-{case}");
        let kind = if case.starts_with("rtmp") {
            "rtmp"
        } else if case.starts_with("srt") {
            "srt"
        } else if case.starts_with("rist") {
            "rist"
        } else if case == "rtsp-userinfo" {
            "rtsp"
        } else {
            "hls"
        };
        let resp = send(
            &h.router,
            post_json(
                &format!("/api/v1/sources/{id}"),
                OPERATOR_TOKEN,
                &json!({
                    "name": case,
                    "body": { "kind": kind, "id": id, "url": raw_url }
                }),
            ),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "{case}: source seed must land: {:?}",
            body_json(resp).await
        );

        let marker = secret_marker_for(case);

        // GET /sources/{id}
        let resp = send(
            &h.router,
            get(&format!("/api/v1/sources/{id}"), VIEWER_TOKEN),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains(marker),
            "{case}: the credential leaked in GET /sources/{{id}}: {text}"
        );

        // GET /sources (list)
        let resp = send(&h.router, get("/api/v1/sources", VIEWER_TOKEN)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains(marker),
            "{case}: the credential leaked in GET /sources (list): {text}"
        );

        // An admin still sees the raw credential (operational parity; the
        // stored document itself is untouched).
        let resp = send(
            &h.router,
            get(&format!("/api/v1/sources/{id}"), ADMIN_TOKEN),
        )
        .await;
        let out = body_json(resp).await;
        assert_eq!(
            out["body"]["url"], raw_url,
            "{case}: an admin reads the stored URL verbatim: {out}"
        );
    }
}

/// Export (`GET /api/v1/config/export`, admin-only whole-system artifact) keeps
/// its established config-as-code posture: real URLs survive so the exported
/// document stays reimportable. This is NOT a regression target — the brief
/// requires export to keep full URLs; this test pins that it still does after
/// the read-route fix (the two code paths must not have been conflated).
#[tokio::test]
async fn export_still_keeps_the_real_transport_url_for_config_as_code() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/working",
            OPERATOR_TOKEN,
            &json!({
                "name": "working",
                "body": {
                    "canvas": {
                        "width": 1920, "height": 1080, "fps": "30/1",
                        "pixel_format": "nv12", "background": "#101014",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": [
                        {
                            "id": "a",
                            "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 0.5 },
                            "source": { "input_id": "export-cam" }
                        }
                    ]
                }
            }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "{:?}",
        body_json(resp).await
    );

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/export-cam",
            OPERATOR_TOKEN,
            &json!({
                "name": "Export cam",
                "body": {
                    "kind": "rtsp", "id": "export-cam",
                    "url": "rtsp://camuser:EXPORT_PASS@[2001:db8::20]:554/main"
                }
            }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "{:?}",
        body_json(resp).await
    );

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/export-out",
            OPERATOR_TOKEN,
            &json!({
                "name": "LL-HLS",
                "body": {
                    "id": "export-out", "kind": "ll_hls",
                    "path": "/var/lib/multiview/hls", "codec": "h264"
                }
            }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "{:?}",
        body_json(resp).await
    );

    let resp = send(&h.router, get("/api/v1/config/export", ADMIN_TOKEN)).await;
    let status = resp.status();
    let bytes = body_bytes(resp).await;
    let toml = String::from_utf8(bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "export failed: {toml}");
    assert!(
        toml.contains("rtsp://camuser:EXPORT_PASS@[2001:db8::20]:554/main")
            || toml.contains("[2001:db8::20]:554/main"),
        "export must keep the real transport URL for config-as-code: {toml}"
    );
}
