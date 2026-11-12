//! Issue #317: "a state the control plane accepts is a state the engine can
//! boot". Every command that persists the accepted state (`desired.json`) is
//! driven over HTTP; afterwards either the persisted state passes the start-up
//! admission validator (`multiview_config::boot_admission`, the rule
//! `Pipeline::build` enforces), or the command was refused and `desired.json`
//! is byte-for-byte unchanged and no engine command was sent.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus, AppState, CommandReceiver, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::{body_json, delete_if_match, etag, get, post_json, put_json, send, ADMIN_TOKEN};

/// The mda001 shape: one full-canvas cell bound to the M/E program source.
const BOOT_SWITCHER: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "absolute"
[[sources]]
id = "in_a"
kind = "bars"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/boot-admission-class.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "in_a"
preview_source = "in_b"
"##;

fn boot_text(switcher: bool) -> String {
    if switcher {
        BOOT_SWITCHER.to_owned()
    } else {
        BOOT_SWITCHER[..BOOT_SWITCHER.find("[switcher]").unwrap()].to_owned()
    }
}

/// A 6-cell "NewsOne"-like canvas (3x2 grid of absolute rects).
fn six_cell_body() -> Value {
    let mut cells = Vec::new();
    for i in 0..6_u32 {
        let x = f64::from(i % 3) / 3.0;
        let y = f64::from(i / 3) / 2.0;
        let source = if i % 2 == 0 { "in_a" } else { "in_b" };
        cells.push(json!({
            "id": format!("c{i}"),
            "rect": {"x": x, "y": y, "w": 1.0 / 3.0, "h": 0.5},
            "source": {"input_id": source}
        }));
    }
    json!({"canvas": {"width": 64, "height": 64, "fps": "25/1"}, "layout": {"kind": "absolute"}, "cells": cells})
}

fn one_cell_body(source: &str) -> Value {
    json!({
        "canvas": {"width": 64, "height": 64, "fps": "25/1"},
        "layout": {"kind": "absolute"},
        "cells": [{"id": "pgm", "rect": {"x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0}, "source": {"input_id": source}}]
    })
}

struct Run {
    _dir: tempfile::TempDir,
    path: PathBuf,
    router: Router,
    commands: CommandReceiver,
}

impl Run {
    fn desired_path(&self) -> PathBuf {
        self.path.parent().unwrap().join(".multiview/desired.json")
    }
    fn desired_bytes(&self) -> Vec<u8> {
        std::fs::read(self.desired_path()).unwrap()
    }
    /// "Restart": what the engine would start from must pass start-up admission.
    fn assert_bootable(&self, what: &str) {
        let desired = load_desired(&self.path).unwrap().expect("accepted state");
        if let Err(error) = multiview_config::boot_admission(&desired.config) {
            panic!("{what}: persisted a state the engine cannot boot: {error}");
        }
    }
}

fn state_for(path: &Path, config: &MultiviewConfig) -> (AppState, CommandReceiver) {
    let seeded = multiview_control::seed_resources(config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, receiver) = command_bus(64);
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_running_canvas(multiview_config::LayoutCanvas::new(
        64,
        64,
        "25/1".parse().unwrap(),
    ))
    .with_base_document(serde_json::to_value(config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )));
    (state, receiver)
}

/// A running engine that has already written its first accepted envelope.
async fn running(switcher: bool) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    let text = boot_text(switcher);
    std::fs::write(&path, &text).unwrap();
    let config = MultiviewConfig::load_from_toml(&text).unwrap();
    let (state, commands) = state_for(&path, &config);
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    multiview_control::boot_model::commit_desired(&state, config, layouts)
        .await
        .unwrap();
    let run = Run {
        _dir: dir,
        path,
        router: multiview_control::router(state),
        commands,
    };
    run.assert_bootable("initial commit");
    run
}

async fn store_layout(run: &Run, id: &str, body: &Value) {
    let response = send(
        &run.router,
        post_json(
            &format!("/api/v1/layouts/{id}"),
            ADMIN_TOKEN,
            &json!({"name": id, "body": body}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED, "store layout {id}");
}

async fn apply(run: &Run, id: &str) -> axum::response::Response {
    send(
        &run.router,
        post_json(
            "/api/v1/commands/apply-layout",
            ADMIN_TOKEN,
            &json!({"layout": id}),
        ),
    )
    .await
}

/// The outcome contract for one persisting command.
async fn expect_accepted_or_refused_cleanly(
    run: &mut Run,
    what: &str,
    before: &[u8],
    response: axum::response::Response,
) -> (StatusCode, Value) {
    let status = response.status();
    let body = if status == StatusCode::NO_CONTENT {
        Value::Null
    } else {
        body_json(response).await
    };
    if status.is_success() {
        run.assert_bootable(what);
    } else {
        assert_eq!(
            run.desired_bytes(),
            before,
            "{what}: a refused command must not touch desired.json"
        );
        assert!(
            run.commands.try_drain().is_empty(),
            "{what}: a refused command must not reach the engine"
        );
        run.assert_bootable(what);
    }
    (status, body)
}

fn assert_typed_switcher_refusal(what: &str, status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {body}");
    assert_eq!(
        body["type"], "/problems/layout-not-admissible-with-switcher",
        "{what}: {body}"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .starts_with("The switcher is on:"),
        "{what}: {body}"
    );
}

#[tokio::test]
async fn apply_layout_table_switcher_on_and_off_single_and_multi_cell() {
    // (switcher, body, expect accepted)
    let cases = [
        (true, one_cell_body("in_a"), true),
        (true, six_cell_body(), false),
        (true, one_cell_body("in_b"), false),
        (false, one_cell_body("in_a"), true),
        (false, six_cell_body(), true),
    ];
    for (index, (switcher, body, accepted)) in cases.into_iter().enumerate() {
        let what = format!(
            "case {index}: switcher={switcher} cells={}",
            body["cells"].as_array().unwrap().len()
        );
        let mut run = Box::pin(running(switcher)).await;
        store_layout(&run, "candidate", &body).await;
        let before = run.desired_bytes();
        let response = apply(&run, "candidate").await;
        let (status, problem) =
            expect_accepted_or_refused_cleanly(&mut run, &what, &before, response).await;
        if accepted {
            assert_eq!(status, StatusCode::ACCEPTED, "{what}: {problem}");
        } else {
            assert_typed_switcher_refusal(&what, status, &problem);
        }
    }
}

/// Today's mda001 sequence (#317): switcher on, Layouts → Apply a 6-cell
/// layout. It must be refused, desired.json unchanged byte for byte, nothing
/// sent to the engine (it keeps running the 1-cell Program), and a "restart"
/// from the persisted state must pass start-up admission.
#[tokio::test]
async fn todays_sequence_six_cell_apply_with_switcher_is_refused_and_restart_boots() {
    let mut run = Box::pin(running(true)).await;
    store_layout(&run, "NewsOne", &six_cell_body()).await;
    let before = run.desired_bytes();
    let _ = run.commands.try_drain();
    let response = apply(&run, "NewsOne").await;
    let (status, body) =
        expect_accepted_or_refused_cleanly(&mut run, "NewsOne apply", &before, response).await;
    assert_typed_switcher_refusal("NewsOne apply", status, &body);
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .starts_with("The switcher is on: a multi-cell layout cannot be the Program canvas."));
    assert_eq!(run.desired_bytes(), before, "byte-for-byte unchanged");
    // Restart: rebuild the control plane from disk; the accepted canvas is
    // still the one full-canvas cell and admits.
    let desired = load_desired(&run.path).unwrap().unwrap();
    assert_eq!(desired.config.cells.len(), 1);
    multiview_config::boot_admission(&desired.config).expect("restart boots");
    let (state, _rx) = state_for(&run.path, &desired.config);
    multiview_control::boot_model::restore_accepted_stores(state, &desired)
        .expect("restored control plane");
}

/// The layout editor's "Save & Apply": PUT the layout body (a library edit that
/// does not change the accepted canvas), then apply-layout.
#[tokio::test]
async fn editor_save_and_apply_of_a_multi_cell_layout_is_refused_after_a_harmless_save() {
    let mut run = Box::pin(running(true)).await;
    store_layout(&run, "edited", &one_cell_body("in_a")).await;
    let current = send(&run.router, get("/api/v1/layouts/edited", ADMIN_TOKEN)).await;
    let tag = etag(&current).unwrap();
    let before = run.desired_bytes();
    let saved = send(
        &run.router,
        put_json(
            "/api/v1/layouts/edited",
            ADMIN_TOKEN,
            Some(&tag),
            &json!({"name": "edited", "body": six_cell_body()}),
        ),
    )
    .await;
    let (status, _) =
        expect_accepted_or_refused_cleanly(&mut run, "editor save", &before, saved).await;
    assert_eq!(status, StatusCode::OK, "saving a library entry is allowed");
    let before = run.desired_bytes();
    let response = apply(&run, "edited").await;
    let (status, body) =
        expect_accepted_or_refused_cleanly(&mut run, "editor apply", &before, response).await;
    assert_typed_switcher_refusal("editor apply", status, &body);
}

/// The other persisting commands: sources, outputs, overlays (and their
/// deletes). Each either persists a bootable state or is refused cleanly.
#[tokio::test]
async fn every_other_persisting_command_yields_a_bootable_state_or_a_clean_refusal() {
    let mut run = Box::pin(running(true)).await;
    let steps: Vec<(&str, axum::http::Request<axum::body::Body>, bool)> = vec![
        (
            "source add",
            post_json(
                "/api/v1/sources/in_c",
                ADMIN_TOKEN,
                &json!({"name": "c", "body": {"id": "in_c", "kind": "bars"}}),
            ),
            true,
        ),
        (
            "output add",
            post_json(
                "/api/v1/outputs/extra",
                ADMIN_TOKEN,
                &json!({"name": "extra", "body": {"kind": "hls", "path": "/tmp/boot-admission-extra.m3u8", "codec": "mpeg2video", "segment_ms": 1000}}),
            ),
            true,
        ),
        // An overlay on the Program canvas is a state Pipeline::build refuses
        // while an M/E is declared: refused before persist.
        (
            "overlay add",
            post_json(
                "/api/v1/overlays/ov_clock",
                ADMIN_TOKEN,
                &json!({"name": "c", "body": {"kind": "clock", "target": "canvas"}}),
            ),
            false,
        ),
    ];
    for (what, request, accepted) in steps {
        let before = run.desired_bytes();
        let response = send(&run.router, request).await;
        let (status, body) =
            expect_accepted_or_refused_cleanly(&mut run, what, &before, response).await;
        assert_eq!(status.is_success(), accepted, "{what}: {status} {body}");
        if !accepted {
            assert_typed_switcher_refusal(what, status, &body);
        }
        let _ = run.commands.try_drain();
    }
    // Removing the M/E program source would leave the switcher dangling.
    for (what, uri, accepted) in [
        ("source remove (unused)", "/api/v1/sources/in_c", true),
        (
            "source remove (program source)",
            "/api/v1/sources/in_a",
            false,
        ),
    ] {
        let current = send(&run.router, get(uri, ADMIN_TOKEN)).await;
        let tag = etag(&current);
        let before = run.desired_bytes();
        let response = send(
            &run.router,
            delete_if_match(uri, ADMIN_TOKEN, tag.as_deref()),
        )
        .await;
        let (status, body) =
            expect_accepted_or_refused_cleanly(&mut run, what, &before, response).await;
        assert_eq!(status.is_success(), accepted, "{what}: {status} {body}");
        let _ = run.commands.try_drain();
    }
}
