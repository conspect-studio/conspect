//! `PUT /api/v1/audio-routing` rename/removal propagation (ADR-R005 §4.3):
//! a routing replace that renames or drops a discrete `target_track` an
//! output's `mode = "tracks"` selection carries must rewrite that selection
//! in the SAME atomic commit — the fix for the production deadlock where an
//! operator could not rename a track an output still selected ("selects
//! unknown audio track") without first `PUTting` an intermediate document by
//! hand.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::json;
use support::{body_json, etag, get, put_json, send, ADMIN_TOKEN, SCOPED_TOKEN};

/// A document with the current (pre-rename) routing: `cam1` -> `au-abc`,
/// and ONE hls output in `mode = "tracks"` selecting `["prog", "au-abc", "us-fox"]`
/// — exactly the production shape in the bug report. A second output (`wall`)
/// stays on `mode = "program"` so the no-op path is also exercised.
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
id = "cam1"
kind = "bars"
[[sources]]
id = "cam2"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam1"
[[outputs]]
kind = "hls"
path = "/tmp/audio-routing-propagation.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.audio]
mode = "tracks"
tracks = ["prog", "au-abc", "us-fox"]
[[outputs]]
kind = "hls"
path = "/tmp/audio-routing-propagation-2.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.audio]
mode = "program"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "cam1"
channels = { kind = "stereo" }
target_track = "au-abc"
include_in_program_bus = true
gain_db = 0.0
mute = false
[[audio.routes]]
input_id = "cam2"
channels = { kind = "stereo" }
target_track = "us-fox"
include_in_program_bus = true
gain_db = 0.0
mute = false
"##;

/// Build a router wired with a real `BootModel` (so the atomic desired-state
/// commit path runs) and the `BOOT` document seeded as both Loaded and
/// Running. Returns the router and the boot file path (for reading back
/// `desired.json` via [`load_desired`]); the backing tempdir is leaked for the
/// duration of the test process (acceptable: each test owns a fresh one and
/// the process exits after the suite).
fn harness_with_boot() -> (Router, std::path::PathBuf) {
    harness_with_doc(BOOT)
}

fn harness_with_doc(doc: &str) -> (Router, std::path::PathBuf) {
    // `keep()` consumes the `TempDir` without running its `Drop` cleanup
    // (unlike `mem::forget`, which clippy flags on a `Drop` type) — the
    // directory is intentionally kept for the test's duration.
    let dir = tempfile::tempdir().unwrap().keep();
    let path = dir.join("boot.toml");
    std::fs::write(&path, doc).unwrap();
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
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
    (multiview_control::router(state), path)
}

/// Build the request body that renames `au-abc` -> `us-cnn` (same routes,
/// same sample rate, one `target_track` renamed) — exactly the bug report's
/// `cnn-usa` route edit.
fn renamed_routing_doc() -> serde_json::Value {
    json!({
        "sample_rate_hz": 48_000,
        "routes": [
            {
                "input_id": "cam1",
                "channels": { "kind": "stereo" },
                "target_track": "us-cnn",
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            },
            {
                "input_id": "cam2",
                "channels": { "kind": "stereo" },
                "target_track": "us-fox",
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            }
        ]
    })
}

/// Build the request body that REMOVES `au-abc` entirely (cam1 drops its
/// `target_track`, contributing to the program bus only).
fn removed_routing_doc() -> serde_json::Value {
    json!({
        "sample_rate_hz": 48_000,
        "routes": [
            {
                "input_id": "cam1",
                "channels": { "kind": "stereo" },
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            },
            {
                "input_id": "cam2",
                "channels": { "kind": "stereo" },
                "target_track": "us-fox",
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            }
        ]
    })
}

/// Build a request body that removes BOTH discrete tracks (the first
/// output's selection would otherwise empty).
fn remove_both_tracks_doc() -> serde_json::Value {
    json!({
        "sample_rate_hz": 48_000,
        "routes": [
            {
                "input_id": "cam1",
                "channels": { "kind": "stereo" },
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            },
            {
                "input_id": "cam2",
                "channels": { "kind": "stereo" },
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            }
        ]
    })
}

/// Reproduces the exact production deadlock and its fix: renaming a track an
/// output selects used to fail with "selects unknown audio track"; it now
/// succeeds in one PUT, rewrites the output's selection atomically, and
/// reports it in `outputs_updated`.
#[tokio::test]
async fn renaming_a_selected_track_succeeds_and_rewrites_the_output_atomically() {
    let (router, boot_path) = harness_with_boot();

    let before = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    let old_etag = etag(&before).unwrap();

    let resp = send(
        &router,
        put_json(
            "/api/v1/audio-routing",
            ADMIN_TOKEN,
            Some(&old_etag),
            &renamed_routing_doc(),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a rename must no longer deadlock the save"
    );
    let body = body_json(resp).await;
    assert_eq!(body["routing"]["routes"][0]["target_track"], "us-cnn");

    let updated = body["outputs_updated"]
        .as_array()
        .expect("outputs_updated is reported");
    assert_eq!(updated.len(), 1, "only the referencing output is touched");
    assert_eq!(updated[0]["output"], "output-0");
    assert_eq!(updated[0]["renamed"][0]["from"], "au-abc");
    assert_eq!(updated[0]["renamed"][0]["to"], "us-cnn");
    assert_eq!(updated[0]["removed"].as_array().unwrap().len(), 0);

    // The accepted desired record carries the rewritten selection — the
    // atomic commit, not a side effect the response merely narrates.
    let desired = load_desired(&boot_path).unwrap().unwrap();
    let output0 = &desired.config.outputs[0];
    let selection = output0.audio().expect("audio selection");
    assert_eq!(
        selection.tracks,
        vec!["prog".to_owned(), "us-cnn".to_owned(), "us-fox".to_owned()],
        "the desired record itself carries the renamed track"
    );
    // The untouched second output's program-bus selection is unaffected.
    assert!(desired.config.outputs[1]
        .audio()
        .is_some_and(|sel| sel.mode == multiview_config::OutputAudioMode::Program));

    // A subsequent GET /api/v1/outputs/output-0 reflects the rewritten
    // selection too (the legacy store projection was kept in sync).
    let output_resp = send(&router, get("/api/v1/outputs/output-0", ADMIN_TOKEN)).await;
    assert_eq!(output_resp.status(), StatusCode::OK);
    let output_body = body_json(output_resp).await;
    assert_eq!(
        output_body["body"]["audio"]["tracks"],
        json!(["prog", "us-cnn", "us-fox"])
    );

    // desired.json round-trips the rewritten selection (persistence proof).
    let reloaded = load_desired(&boot_path).unwrap().unwrap();
    assert_eq!(
        reloaded.config.outputs[0].audio().unwrap().tracks,
        vec!["prog".to_owned(), "us-cnn".to_owned(), "us-fox".to_owned()]
    );
}

/// Removing a route's discrete track drops it from every output selection
/// that referenced it, in the same atomic commit.
#[tokio::test]
async fn removing_a_selected_track_drops_it_from_the_referencing_output() {
    let (router, boot_path) = harness_with_boot();
    let before = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    let old_etag = etag(&before).unwrap();

    let resp = send(
        &router,
        put_json(
            "/api/v1/audio-routing",
            ADMIN_TOKEN,
            Some(&old_etag),
            &removed_routing_doc(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let updated = body["outputs_updated"].as_array().unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["removed"], json!(["au-abc"]));
    assert_eq!(updated[0]["renamed"].as_array().unwrap().len(), 0);

    let desired = load_desired(&boot_path).unwrap().unwrap();
    assert_eq!(
        desired.config.outputs[0].audio().unwrap().tracks,
        vec!["prog".to_owned(), "us-fox".to_owned()]
    );
}

/// An output whose selection would otherwise empty after a removal falls
/// back to `["prog"]` rather than landing an invalid empty `tracks` list.
#[tokio::test]
async fn removing_every_discrete_track_falls_back_to_the_program_bus() {
    let (router, boot_path) = harness_with_boot();
    let before = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    let old_etag = etag(&before).unwrap();

    let resp = send(
        &router,
        put_json(
            "/api/v1/audio-routing",
            ADMIN_TOKEN,
            Some(&old_etag),
            &remove_both_tracks_doc(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let updated = body["outputs_updated"].as_array().unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(
        updated[0]["removed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<std::collections::HashSet<_>>(),
        ["au-abc", "us-fox"].into_iter().collect()
    );

    let desired = load_desired(&boot_path).unwrap().unwrap();
    assert_eq!(
        desired.config.outputs[0].audio().unwrap().tracks,
        vec!["prog".to_owned()],
        "an emptied selection falls back to the program bus"
    );
}

/// A principal not authorized to modify the referenced output is refused —
/// the routing document is left untouched (no partial commit).
#[tokio::test]
async fn a_rename_touching_an_unauthorized_output_is_refused_and_nothing_is_changed() {
    let (router, boot_path) = harness_with_boot();
    let before = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    let old_etag = etag(&before).unwrap();

    // SCOPED_TOKEN's object allowlist is `["scoped-layout"]` — it may not
    // touch `output-0`, which this rename would rewrite.
    let resp = send(
        &router,
        put_json(
            "/api/v1/audio-routing",
            SCOPED_TOKEN,
            Some(&old_etag),
            &renamed_routing_doc(),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an out-of-scope output in the propagation set is refused"
    );
    let problem = body_json(resp).await;
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or("")
            .contains("output-0"),
        "the detail names the blocked output: {problem}"
    );

    // Nothing was committed: no desired record was published at all (the
    // first-ever successful commit never landed), the OLD etag still
    // satisfies a subsequent GET, and the referenced output still carries
    // its ORIGINAL track name.
    assert!(
        load_desired(&boot_path).unwrap().is_none(),
        "a refused propagation must not publish any desired record"
    );
    let after = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    assert_eq!(etag(&after).unwrap(), old_etag);
    let output_after =
        body_json(send(&router, get("/api/v1/outputs/output-0", ADMIN_TOKEN)).await).await;
    assert_eq!(
        output_after["body"]["audio"]["tracks"],
        json!(["prog", "au-abc", "us-fox"]),
        "the output's selection must be untouched by the refused PUT"
    );
}

/// A routing replace that touches no output's selection (e.g. only a gain
/// change) is unaffected by the propagation machinery: no `outputs_updated`,
/// no output store writes.
#[tokio::test]
async fn an_unrelated_routing_change_reports_no_output_updates() {
    let (router, _boot_path) = harness_with_boot();
    let before = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    let old_etag = etag(&before).unwrap();

    let doc = json!({
        "sample_rate_hz": 48_000,
        "routes": [
            {
                "input_id": "cam1",
                "channels": { "kind": "stereo" },
                "target_track": "au-abc",
                "include_in_program_bus": true,
                "gain_db": -6.0,
                "mute": false
            },
            {
                "input_id": "cam2",
                "channels": { "kind": "stereo" },
                "target_track": "us-fox",
                "include_in_program_bus": true,
                "gain_db": 0.0,
                "mute": false
            }
        ]
    });
    let resp = send(
        &router,
        put_json("/api/v1/audio-routing", ADMIN_TOKEN, Some(&old_etag), &doc),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body.get("outputs_updated").is_none()
            || body["outputs_updated"].as_array().unwrap().is_empty(),
        "a gain-only change must not report any output update"
    );
}

/// ADR-0108 §5.5: a `[[routing.output]] audio = { tracks = [..] }` feed
/// follows a rename in the same atomic commit, and is reported in
/// `outputs_updated` like an output's own selection.
#[tokio::test]
async fn a_routed_output_audio_feed_follows_a_renamed_track() {
    let doc = BOOT.replacen(
        "[[outputs]]\nkind = \"hls\"\npath = \"/tmp/audio-routing-propagation-2.m3u8\"",
        "[[outputs]]\nid = \"wall\"\nkind = \"hls\"\npath = \"/tmp/audio-routing-propagation-2.m3u8\"",
        1,
    ) + "[[routing.output]]\noutput = \"wall\"\naudio = { tracks = [\"au-abc\"] }\n";
    assert!(doc.contains("id = \"wall\""));
    let (router, boot_path) = harness_with_doc(&doc);
    let before = send(&router, get("/api/v1/audio-routing", ADMIN_TOKEN)).await;
    let old_etag = etag(&before).unwrap();
    let resp = send(
        &router,
        put_json(
            "/api/v1/audio-routing",
            ADMIN_TOKEN,
            Some(&old_etag),
            &renamed_routing_doc(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let updated = body["outputs_updated"].as_array().unwrap();
    let wall = updated
        .iter()
        .find(|u| u["output"] == "wall")
        .unwrap_or_else(|| panic!("wall reported: {updated:?}"));
    assert_eq!(wall["renamed"][0]["from"], "au-abc");
    assert_eq!(wall["renamed"][0]["to"], "us-cnn");
    let desired = load_desired(&boot_path).unwrap().unwrap();
    let feed = desired
        .config
        .output_feeds()
        .into_iter()
        .find(|f| f.output == "wall")
        .unwrap();
    assert_eq!(
        feed.audio,
        multiview_config::AudioFeed::Tracks(vec!["us-cnn".to_owned()]),
        "the desired record carries the renamed feed"
    );
}
