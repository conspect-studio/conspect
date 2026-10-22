//! `GET /api/v1/config/export` (ADR-W015): compose the live resource stores
//! (working layout + sources + outputs + overlays) into a full
//! `MultiviewConfig` document and return it as TOML, closing the UI → config
//! file loop. The composed document is validated as a whole before render.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{
    body_json, get, harness, post_json, put_json, send, OPERATOR_TOKEN, SCOPED_TOKEN, VIEWER_TOKEN,
};

/// Seed a working layout + one source + one output through the public API,
/// mirroring what `seed_resources` does for a config-driven run.
async fn seed(h: &support::Harness) {
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/working",
            OPERATOR_TOKEN,
            &json!({
                "name": "working",
                "body": {
                    "canvas": {
                        "width": 1920,
                        "height": 1080,
                        "fps": "30/1",
                        "pixel_format": "nv12",
                        "background": "#101014",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": [
                        {
                            "id": "a",
                            "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 0.5 },
                            "source": { "input_id": "cam1" }
                        }
                    ]
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "layout seed must land");

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam1" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "source seed must land");

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/web1",
            OPERATOR_TOKEN,
            &json!({
                "name": "LL-HLS",
                "body": { "id": "web1", "kind": "ll_hls", "path": "/var/lib/multiview/hls", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "output seed must land");
}

/// BOLA wholesale-enumeration leak (OWASP API1, ADR-W005/ADR-W025): the full
/// config export is a whole-system desired-state document — every device, every
/// `device_ref`, every sync-group member id. An **object-scoped** principal must
/// NOT be able to export it: it would disclose the ids of every object outside
/// its allowlist in one call, bypassing per-object authorization wholesale. The
/// export is meaningful only for a principal that can see the whole system.
///
/// `SCOPED_TOKEN` (allowlist `["scoped-layout"]`) is denied `403`; an unscoped
/// reader still exports (no over-restriction).
#[tokio::test]
async fn scoped_principal_cannot_export_the_whole_config() {
    let h = harness();
    seed(&h).await;

    // A scoped principal must be forbidden the whole-system export.
    let resp = send(&h.router, get("/api/v1/config/export", SCOPED_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not export the whole-system config (BOLA wholesale leak)"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");

    // An unscoped reader still exports (the guard does not over-restrict).
    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unscoped reader may still export the config"
    );
}

#[tokio::test]
async fn export_renders_the_stores_as_valid_toml() {
    let h = harness();
    seed(&h).await;

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .expect("export has a content type")
            .to_str()
            .unwrap(),
        "application/toml"
    );

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    // The document must itself round-trip through the canonical config type.
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export is a valid MultiviewConfig document");
    assert_eq!(parsed.canvas.width, 1920);
    assert_eq!(parsed.sources.len(), 1, "the created source is exported");
    assert_eq!(parsed.sources[0].id, "cam1");
    assert_eq!(parsed.outputs.len(), 1, "the created output is exported");
    assert_eq!(
        parsed.cells.len(),
        1,
        "the working layout cells are exported"
    );
}

#[tokio::test]
async fn export_carries_a_created_probe() {
    let h = harness();
    seed(&h).await;

    // A probe watching the seeded cell "a" (document-level validation resolves
    // the cell reference, so the export only composes when it exists).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/black-a",
            OPERATOR_TOKEN,
            &json!({
                "name": "Black on a",
                "body": {
                    "cell": "a",
                    "kind": "black",
                    "luma_threshold": 16,
                    "dwell": { "up_ms": 2000, "down_ms": 1000 },
                    "severity": "Major"
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "probe seed must land");

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export is a valid MultiviewConfig document");
    assert_eq!(parsed.probes.len(), 1, "the created probe is exported");
    assert_eq!(parsed.probes[0].id, "black-a");
    assert_eq!(parsed.probes[0].cell, "a");
}

#[tokio::test]
async fn export_requires_authentication() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/config/export")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_without_a_working_layout_is_422() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = support::body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn export_retains_base_config_sections_the_stores_do_not_carry() {
    // Review B1: exporting must not destroy authored sections (control,
    // placement, probes, …) — a restart with the exported file would otherwise
    // lose the management listener itself.
    let base = json!({
        "schema_version": 1,
        "canvas": {
            "width": 1280, "height": 720, "fps": "25/1",
            "pixel_format": "nv12", "background": "#000000",
            "color": { "profile": "sdr-bt709-limited" }
        },
        "layout": { "kind": "absolute" },
        "cells": [],
        "sources": [],
        "outputs": [ { "kind": "hls", "path": "/srv/hls", "codec": "h264" } ],
        "control": { "listen": "[::1]:8087" },
        "placement": { "reserve_headroom": 0.2 },
        "probes": []
    });
    let h = support::harness_with(|state| state.with_base_document(base.clone()));
    seed(&h).await;

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        text.contains("[control]") && text.contains("[::1]:8087"),
        "the [control] section survives export:\n{text}"
    );
    assert!(text.contains("[placement"), "placement survives export");
    // Store-backed sections override the base: the seeded source/output/canvas
    // win over the base document's.
    let parsed: multiview_config::MultiviewConfig = toml::from_str(&text).unwrap();
    assert_eq!(parsed.canvas.width, 1920, "working-layout canvas wins");
    assert_eq!(parsed.sources.len(), 1, "store sources win");
    assert_eq!(parsed.outputs.len(), 1, "store outputs win");
}

#[tokio::test]
async fn export_round_trips_the_system_ndi_acceptance_as_a_flag() {
    // ADR-0008 §7.5: the NDI license acceptance is an authored [system] section no
    // store carries; the export must pass it through verbatim and "as a flag, never
    // a secret" (visible in the rendered TOML, not redacted) so the legal acceptance
    // travels with the config. Guards the verbatim base_document passthrough against
    // future export refactors that reconstruct sections explicitly.
    let base = json!({
        "system": {
            "ndi": {
                "accept_license": true,
                "accepted_by": "operator@example",
                "accepted_at": "2026-06-06T00:00:00Z"
            }
        }
    });
    let h = support::harness_with(|state| state.with_base_document(base.clone()));
    seed(&h).await;

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    // Round-trips through the canonical type with the acceptance intact.
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export is a valid MultiviewConfig document");
    let ndi = parsed
        .system
        .as_ref()
        .and_then(|s| s.ndi.as_ref())
        .expect("the [system.ndi] acceptance survives the export round-trip");
    assert!(ndi.accept_license);
    assert_eq!(ndi.accepted_by.as_deref(), Some("operator@example"));

    // Exported as a plain flag, never a secret (not redacted / behind a secret_ref).
    assert!(
        text.contains("accept_license = true"),
        "the acceptance flag must export plainly:\n{text}"
    );
    assert!(
        text.contains("operator@example"),
        "the audit principal must export plainly (not redacted):\n{text}"
    );
}

#[tokio::test]
async fn export_prefers_the_seeded_working_layout_over_alphabetical_order() {
    // Review M1: with several layouts carrying a canvas, the export must use
    // the designated working layout, not the id-sorted first.
    let h = support::harness_with(|state| state.with_working_layout_id("schema_v1"));
    seed(&h).await; // seeds layout id "working" (carries 1920x1080)

    // An alphabetically-earlier decoy with a different canvas.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/aaa-decoy",
            OPERATOR_TOKEN,
            &json!({
                "name": "decoy",
                "body": {
                    "canvas": {
                        "width": 640, "height": 360, "fps": "25/1",
                        "pixel_format": "nv12", "background": "#000000",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": []
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The designated working layout id ("schema_v1") doesn't exist yet — so
    // create it too, mirroring the seeded-name flow.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/schema_v1",
            OPERATOR_TOKEN,
            &json!({
                "name": "schema_v1",
                "body": {
                    "canvas": {
                        "width": 3840, "height": 2160, "fps": "30/1",
                        "pixel_format": "nv12", "background": "#101014",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": []
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(core::str::from_utf8(&body).unwrap()).unwrap();
    assert_eq!(
        parsed.canvas.width, 3840,
        "the designated working layout wins over the alphabetical decoy"
    );
}

#[tokio::test]
async fn export_overlays_the_stored_audio_routing_when_configured() {
    // The audio-routing singleton (PUT /api/v1/audio-routing) is part of the
    // composed document: when configured it overlays the `audio` key, and the
    // whole-document validation cross-checks its routes against the declared
    // sources (the check the PUT boundary intentionally defers).
    let h = harness();
    seed(&h).await; // declares source "cam1"

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/audio-routing",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    {
                        "input_id": "cam1",
                        "channels": { "kind": "stereo" },
                        "target_track": "cam1-clean",
                        "include_in_program_bus": true,
                        "gain_db": -3.0
                    }
                ]
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "audio routing PUT must land");

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export with audio is a valid document");
    let audio = parsed
        .audio
        .as_ref()
        .expect("the [audio] block is exported");
    assert_eq!(audio.sample_rate_hz, 48_000);
    assert_eq!(audio.routes.len(), 1);
    assert_eq!(audio.routes[0].input_id, "cam1");
    assert_eq!(audio.routes[0].target_track.as_deref(), Some("cam1-clean"));
}

#[tokio::test]
async fn export_rejects_audio_routes_bound_to_undeclared_sources() {
    // PUT accepts a route naming a not-yet-declared source (internal
    // consistency only); the EXPORT is where the whole document is validated,
    // so the dangling reference must surface as 422 here.
    let h = harness();
    seed(&h).await; // declares only "cam1"

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/audio-routing",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &json!({
                "sample_rate_hz": 48_000,
                "routes": [
                    {
                        "input_id": "ghost-cam",
                        "channels": { "kind": "stereo" },
                        "include_in_program_bus": true
                    }
                ]
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = support::body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or("")
            .contains("ghost-cam"),
        "the violation names the dangling source, got: {}",
        problem["detail"]
    );
}

#[tokio::test]
async fn export_preserves_the_base_documents_audio_when_the_store_is_unconfigured() {
    // An authored [audio] block in the loaded config must survive an export
    // round-trip untouched when no operator edit replaced it.
    let base = json!({
        "schema_version": 1,
        "canvas": {
            "width": 1280, "height": 720, "fps": "25/1",
            "pixel_format": "nv12", "background": "#000000",
            "color": { "profile": "sdr-bt709-limited" }
        },
        "layout": { "kind": "absolute" },
        "cells": [],
        "sources": [ { "id": "cam1", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam1" } ],
        "outputs": [],
        "audio": {
            "sample_rate_hz": 96_000,
            "routes": [
                { "input_id": "cam1", "channels": { "kind": "stereo" },
                  "include_in_program_bus": true }
            ]
        }
    });
    let h = support::harness_with(|state| state.with_base_document(base.clone()));
    seed(&h).await;

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(core::str::from_utf8(&body).unwrap()).unwrap();
    let audio = parsed
        .audio
        .as_ref()
        .expect("the authored [audio] survives");
    assert_eq!(
        audio.sample_rate_hz, 96_000,
        "the base document's audio block is untouched"
    );
}

#[tokio::test]
async fn export_carries_a_download_disposition() {
    let h = harness();
    seed(&h).await;
    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-disposition")
            .expect("export offers a filename")
            .to_str()
            .unwrap(),
        "attachment; filename=\"multiview.toml\""
    );
}

#[tokio::test]
async fn export_redacts_webrtc_ice_secrets_and_source_tokens() {
    // SECURITY: `GET /config/export` renders the live stores + the authored base
    // document. WebRTC ICE credentials (`password` long-term + `static_auth_secret`
    // ephemeral-REST) and a WHIP-source bearer `token` are plaintext secrets that
    // must NEVER appear in the exported document. They are redacted to a clear
    // sentinel, and the redacted document is still a structurally valid
    // MultiviewConfig (a TURN server keeps a non-empty credential field, so it
    // round-trips as a clearly-marked placeholder rather than clobbering nothing).
    const ICE_PASSWORD: &str = "sup3r-secret-turn-password";
    const ICE_AUTH_SECRET: &str = "coturn-shared-rest-secret-9z";
    const SOURCE_TOKEN: &str = "whip-bearer-token-abc123";

    let base = json!({
        "schema_version": 1,
        "canvas": {
            "width": 1280, "height": 720, "fps": "25/1",
            "pixel_format": "nv12", "background": "#000000",
            "color": { "profile": "sdr-bt709-limited" }
        },
        "layout": { "kind": "absolute" },
        "cells": [],
        "sources": [],
        "outputs": [ { "kind": "hls", "path": "/srv/hls", "codec": "h264" } ],
        "webrtc": {
            "ice_servers": [
                {
                    "kind": "turn",
                    "url": "turn:[2001:db8::55]:3478",
                    "username": "publisher",
                    "password": ICE_PASSWORD
                },
                {
                    "kind": "turn",
                    "url": "turns:[2001:db8::56]:5349",
                    "username": "ephemeral",
                    "static_auth_secret": ICE_AUTH_SECRET
                }
            ]
        }
    });
    let h = support::harness_with(|state| state.with_base_document(base.clone()));
    seed(&h).await;

    // A WHIP source carrying a plaintext bearer token (another inline secret).
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
    assert_eq!(resp.status(), StatusCode::CREATED, "whip source seed lands");

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    // No plaintext secret VALUE may appear anywhere in the exported document.
    assert!(
        !text.contains(ICE_PASSWORD),
        "the ICE long-term password leaked in the export:\n{text}"
    );
    assert!(
        !text.contains(ICE_AUTH_SECRET),
        "the ICE static_auth_secret leaked in the export:\n{text}"
    );
    assert!(
        !text.contains(SOURCE_TOKEN),
        "the WHIP source bearer token leaked in the export:\n{text}"
    );

    // The redaction sentinel marks where a secret was removed.
    assert!(
        text.contains("<redacted>"),
        "the export carries the redaction sentinel:\n{text}"
    );

    // The redacted document is still a valid MultiviewConfig: the TURN servers
    // keep a non-empty (sentinel) credential, so validation passes and the
    // document round-trips as a clearly-marked placeholder.
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("the redacted export is still a valid document");
    let ice = &parsed.webrtc.ice_servers;
    assert_eq!(ice.len(), 2, "both TURN servers survive (structure intact)");
    assert_eq!(
        ice[0].password.as_deref(),
        Some("<redacted>"),
        "the long-term password is the sentinel, not the cleartext"
    );
    assert_eq!(
        ice[1].static_auth_secret.as_deref(),
        Some("<redacted>"),
        "the ephemeral-REST secret is the sentinel, not the cleartext"
    );
}

#[tokio::test]
async fn the_export_segment_is_reserved_in_the_versioning_namespace() {
    // `/api/v1/config/export` (static) wins over `/config/{target}` — the
    // literal target name "export" is reserved by design (ADR-W015): GET
    // returns the export document, and committing to a target named "export"
    // is a 405, never a silent versioning write.
    let h = harness();
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/export",
            OPERATOR_TOKEN,
            None,
            &json!({ "document": {}, "message": "nope" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn export_rejects_a_probe_watching_an_unknown_cell() {
    // A probe may be stored against any non-empty cell id (per-item checks
    // cannot see the layout), but the composed export enforces the reference:
    // a probe watching a cell the working layout does not declare is a named
    // 422 for every export caller.
    let h = harness();
    seed(&h).await;
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/probes/ghost",
            OPERATOR_TOKEN,
            &json!({
                "name": "Ghost watcher",
                "body": { "cell": "no-such-cell", "kind": "black", "luma_threshold": 16 }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "stored per-item");

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = support::body_json(resp).await;
    let detail = problem["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("no-such-cell") || detail.contains("ghost"),
        "the violation names the probe/cell, got: {detail}"
    );
}

#[tokio::test]
async fn export_carries_a_runtime_adopted_device_and_sync_group() {
    // ADR-M008: config-as-code is the durable source. A device (and a
    // sync group over it) adopted at runtime via the API MUST round-trip
    // through `GET /config/export`, or the adoption is silently lost on
    // restart. Runtime status (the device_status registry) must NOT leak in.
    let h = harness();
    seed(&h).await;

    for (id, addr) in [
        ("dev-node-left", "http://[fd00:db8::1]"),
        ("dev-node-right", "http://[fd00:db8::2]"),
    ] {
        let resp = send(
            &h.router,
            post_json(
                &format!("/api/v1/devices/{id}"),
                OPERATOR_TOKEN,
                &json!({ "name": id, "body": { "id": id, "driver": "displaynode" } }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "device {id} must adopt");
        let _ = addr;
    }

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall",
            OPERATOR_TOKEN,
            &json!({
                "name": "Lobby wall",
                "body": {
                    "id": "lobby-wall",
                    "mode": "auto",
                    "target_skew_ms": 50,
                    "members": [
                        { "device": "dev-node-left", "offset_ms": 0 },
                        { "device": "dev-node-right", "offset_ms": 120 }
                    ]
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "sync group must create");

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export is a valid MultiviewConfig document");

    assert_eq!(parsed.devices.len(), 2, "both adopted devices are exported");
    assert!(
        parsed.devices.iter().any(|d| d.id == "dev-node-left"),
        "the adopted device id round-trips"
    );
    assert_eq!(parsed.sync_groups.len(), 1, "the sync group is exported");
    assert_eq!(parsed.sync_groups[0].id, "lobby-wall");
    assert_eq!(parsed.sync_groups[0].members.len(), 2);
    // Runtime status never leaks into config-as-code: a MultiviewConfig has no
    // device-status field at all, so a clean parse already proves the boundary.
}
