//! Issue #519: adding a source live must never drop another accepted source,
//! and no accepted-state removal is ever silent.
//!
//! On mda001 (17 Nov) `desired.json` lost `youtube-germnay` around the time
//! `vt-1-src` (`kind = "media_player"`) was added live. The envelope showed a
//! DELETE-shaped removal (name dropped, version tombstoned) that no persistent
//! log recorded. These tests pin:
//! * a live `media_player` POST on a switcher run with N sources persists
//!   N + 1 sources, with every earlier source, name and version unchanged;
//! * an explicit DELETE is audited under the caller;
//! * a commit that drops a resource it did not name (a whole-document commit)
//!   is audited as an implicit `delete` by the `accepted-state` actor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_desired, BootModel, IMPLICIT_REMOVAL_ACTOR};
use multiview_control::{AppState, AuditAction, Command};
use serde_json::json;
use support::{
    delete_if_match, etag, get, harness_with, post_json, send, Harness, ADMIN_TOKEN, OPERATOR_TOKEN,
};

/// A switcher run shaped like mda001's: many decoded sources, a synthetic
/// one, two media players and an M/E on Program/Preview.
const DOC: &str = r##"
schema_version = 1
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
id = "au-abc-news"
kind = "hls"
url = "http://[::1]:9/au.m3u8"
[[sources]]
id = "clock"
kind = "clock"
[[sources]]
id = "dw-news"
kind = "hls"
url = "http://[::1]:9/dw.m3u8"
activation = "on_demand"
[[sources]]
id = "bbc-news"
kind = "hls"
url = "http://[::1]:9/bbc.m3u8"
[[sources]]
id = "cnn-usa"
kind = "hls"
url = "http://[::1]:9/cnn.m3u8"
[[sources]]
id = "youtube-germnay"
kind = "hls"
url = "http://[::1]:9/germany.m3u8"
display_name = "WebCam Germany"
[[media_players]]
id = "vt-1"
loop_default = true
[[media_players]]
id = "vt-2"
loop_default = true
[[cells]]
id = "pgm"
area = "a"
[cells.source]
input_id = "au-abc-news"
[[outputs]]
kind = "hls"
path = "/tmp/accepted-source-integrity/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "au-abc-news"
preview_source = "bbc-news"
"##;

struct Rig {
    h: Harness,
    state: AppState,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    config.validate().unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conspect.toml");
    std::fs::write(&path, DOC).unwrap();
    let boot = path.clone();
    let kept = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&kept);
    let h = harness_with(move |state| {
        let state = state
            .with_seeded_resources(seeded)
            .with_base_document(serde_json::to_value(&config).unwrap())
            .with_boot_model(Arc::new(BootModel::new(
                boot,
                config.clone(),
                config,
                StartMode::Boot,
                false,
                None,
            )))
            .with_live_sources(multiview_control::LiveSourceCapability::synthetic_and_network());
        *slot.lock().unwrap() = Some(state.clone());
        state
    });
    let state = kept.lock().unwrap().take().expect("state");
    Rig {
        h,
        state,
        path,
        _dir: dir,
    }
}

/// The persisted accepted source ids, in document order.
fn persisted_ids(rig: &Rig) -> Vec<String> {
    load_desired(&rig.path)
        .unwrap()
        .expect("an accepted envelope")
        .config
        .sources
        .iter()
        .map(|s| s.id.clone())
        .collect()
}

async fn audit(rig: &Rig) -> Vec<serde_json::Value> {
    let resp = send(&rig.h.router, get("/api/v1/audit", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    support::body_json(resp)
        .await
        .as_array()
        .cloned()
        .unwrap_or_default()
}

async fn post_source(rig: &Rig, id: &str, name: &str, body: serde_json::Value) -> StatusCode {
    let resp = send(
        &rig.h.router,
        post_json(
            &format!("/api/v1/sources/{id}"),
            OPERATOR_TOKEN,
            &json!({ "name": name, "body": body }),
        ),
    )
    .await;
    let status = resp.status();
    if status == StatusCode::CREATED {
        assert_eq!(resp.headers()["x-multiview-apply"], "live", "{id}");
    }
    status
}

/// Commit a first, unrelated change so the run has an accepted envelope (the
/// mda001 state had one long before the add).
async fn seed_envelope(rig: &Rig) {
    assert_eq!(
        post_source(
            rig,
            "solid-1",
            "Solid",
            json!({"id": "solid-1", "kind": "solid", "color": "#202020"})
        )
        .await,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn live_media_player_add_on_a_switcher_run_keeps_every_accepted_source() {
    let mut rig = rig();
    seed_envelope(&rig).await;
    let before_doc = load_desired(&rig.path).unwrap().unwrap();
    let before = persisted_ids(&rig);
    let n = before.len();
    assert!(before.contains(&"youtube-germnay".to_owned()));

    assert_eq!(
        post_source(
            &rig,
            "vt-1-src",
            "VT 1 (Windy)",
            json!({"id": "vt-1-src", "kind": "media_player", "player": "vt-1", "display_name": "VT 1 (Windy)"}),
        )
        .await,
        StatusCode::CREATED
    );
    let upserts = rig
        .h
        .commands
        .try_drain()
        .into_iter()
        .filter(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == "vt-1-src"))
        .count();
    assert_eq!(upserts, 1, "the alias is applied live");

    let after_doc = load_desired(&rig.path).unwrap().unwrap();
    let after = persisted_ids(&rig);
    assert_eq!(after.len(), n + 1, "persisted set is N + 1: {after:?}");
    let mut expected = before.clone();
    expected.push("vt-1-src".to_owned());
    assert_eq!(after, expected, "nothing removed or reordered");
    for id in &before {
        assert_eq!(
            after_doc.resource_names["sources"].get(id),
            before_doc.resource_names["sources"].get(id),
            "{id}: name unchanged"
        );
        assert_eq!(
            after_doc.resource_versions["sources"].get(id),
            before_doc.resource_versions["sources"].get(id),
            "{id}: version unchanged"
        );
    }
    assert_eq!(
        after_doc.resource_names["sources"]
            .get("vt-1-src")
            .map(String::as_str),
        Some("VT 1 (Windy)")
    );
    // Every earlier source is still served.
    for id in &before {
        let resp = send(
            &rig.h.router,
            get(&format!("/api/v1/sources/{id}"), OPERATOR_TOKEN),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
    }
    // And nothing was audited as a delete.
    let entries = audit(&rig).await;
    assert!(
        !entries.iter().any(|e| e["action"] == "delete"),
        "no removal: {entries:?}"
    );
}

/// The second player, and a repeated add of another alias, still only append.
#[tokio::test]
async fn several_live_media_player_adds_only_append() {
    let rig = rig();
    seed_envelope(&rig).await;
    let n = persisted_ids(&rig).len();
    for (id, player) in [
        ("vt-1-src", "vt-1"),
        ("vt-2-src", "vt-2"),
        ("vt-1-b", "vt-1"),
    ] {
        assert_eq!(
            post_source(
                &rig,
                id,
                id,
                json!({"id": id, "kind": "media_player", "player": player})
            )
            .await,
            StatusCode::CREATED,
            "{id}"
        );
    }
    let after = persisted_ids(&rig);
    assert_eq!(after.len(), n + 3, "{after:?}");
    assert!(after.contains(&"youtube-germnay".to_owned()));
}

/// An explicit DELETE is audited under the caller (the trail issue #519 lacked).
#[tokio::test]
async fn explicit_delete_is_audited_under_the_caller() {
    let rig = rig();
    seed_envelope(&rig).await;
    let current = send(
        &rig.h.router,
        get("/api/v1/sources/youtube-germnay", ADMIN_TOKEN),
    )
    .await;
    let tag = etag(&current).unwrap();
    let resp = send(
        &rig.h.router,
        delete_if_match("/api/v1/sources/youtube-germnay", ADMIN_TOKEN, Some(&tag)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(!persisted_ids(&rig).contains(&"youtube-germnay".to_owned()));
    let entries = audit(&rig).await;
    let deletes: Vec<_> = entries
        .iter()
        .filter(|e| e["action"] == "delete" && e["object_id"] == "youtube-germnay")
        .collect();
    assert_eq!(deletes.len(), 1, "{entries:?}");
    assert_eq!(deletes[0]["actor"], "admin-key");
    assert!(
        !entries.iter().any(|e| e["actor"] == IMPLICIT_REMOVAL_ACTOR),
        "an explicit delete is not reported as implicit: {entries:?}"
    );
}

/// A commit that drops a source it did not name is never silent: it is
/// audited as an implicit `delete` by the `accepted-state` actor.
#[tokio::test]
async fn whole_document_commit_that_drops_a_source_is_audited_as_implicit() {
    let rig = rig();
    seed_envelope(&rig).await;
    let state = rig.state.clone();
    let mutation = state.lock_config_mutation().await;
    let mut next: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    next.sources.retain(|s| s.id != "youtube-germnay");
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    multiview_control::boot_model::commit_desired(&state, next, layouts)
        .await
        .unwrap();
    drop(mutation);
    assert!(!persisted_ids(&rig).contains(&"youtube-germnay".to_owned()));
    let implicit: Vec<_> = state
        .audit
        .list(Some("youtube-germnay"))
        .unwrap()
        .into_iter()
        .filter(|e| e.actor == IMPLICIT_REMOVAL_ACTOR)
        .collect();
    assert_eq!(implicit.len(), 1, "{implicit:?}");
    assert_eq!(implicit[0].action, AuditAction::Delete);
    assert_eq!(implicit[0].object_kind, "source");
    assert_eq!(implicit[0].detail.as_ref().unwrap()["implicit"], true);
}
