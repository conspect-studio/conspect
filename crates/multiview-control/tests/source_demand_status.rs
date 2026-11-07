//! Source demand status (`GET /api/v1/sources/status`, `/sources/{id}/status`)
//! and the honest "not probed yet" stream inventory for an idle source
//! (`docs/architecture/demand-driven-sources.md`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use multiview_control::{
    SourceActivationView, SourceConsumerView, SourceDemandLease, SourceDemandPhase,
    SourceDemandProvider, SourceDemandView, StopRefusal,
};
use serde_json::json;
use support::{
    body_json, get, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

/// A fake demand manager: `cam` is ready under one cell, everything else idle.
#[derive(Default)]
struct FakeDemand {
    touched: Mutex<Vec<String>>,
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
}

impl SourceDemandProvider for FakeDemand {
    fn status(&self, id: &str) -> SourceDemandView {
        self.active()
            .into_iter()
            .find(|view| view.id == id)
            .unwrap_or_else(|| SourceDemandView::idle(id))
    }
    fn active(&self) -> Vec<SourceDemandView> {
        vec![SourceDemandView {
            id: "cam".into(),
            phase: SourceDemandPhase::Ready,
            video_wanted: true,
            audio_wanted: false,
            video_running: true,
            audio_running: false,
            consumers: vec![SourceConsumerView {
                kind: "layout_cell".into(),
                label: "cell-1".into(),
            }],
            detail: None,
            starting_phase: None,
            deadline_in_ms: None,
            activation: SourceActivationView::AlwaysOn,
        }]
    }
    fn preview_lease(&self, _id: &str, _label: &str) -> Option<Box<dyn SourceDemandLease>> {
        None
    }
    fn touch_preview(&self, id: &str) -> Option<SourceDemandPhase> {
        self.touched.lock().unwrap().push(id.to_owned());
        Some(SourceDemandPhase::Starting)
    }
    fn start_source(&self, id: &str) -> bool {
        self.started.lock().unwrap().push(id.to_owned());
        true
    }
    fn stop_source(&self, id: &str) -> Result<(), StopRefusal> {
        if id == "cam" {
            return Err(StopRefusal::InUse(vec![SourceConsumerView {
                kind: "layout_cell".into(),
                label: "cell-1".into(),
            }]));
        }
        self.stopped.lock().unwrap().push(id.to_owned());
        Ok(())
    }
}

fn base_document() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "canvas": {"width": 640, "height": 360, "fps": "25/1", "pixel_format": "nv12",
                   "background": "#000000", "color": {"profile": "sdr-bt709-limited"}},
        "layout": {"kind": "grid", "columns": ["1fr"], "rows": ["1fr"], "areas": ["a"]},
        "sources": [
            {"id": "cam", "kind": "bars"},
            {"id": "idle", "kind": "hls", "url": "https://example.test/i.m3u8", "activation": "on_demand"}
        ],
        "cells": [{"id": "cell", "area": "a", "source": {"input_id": "cam"}}],
        "outputs": []
    })
}

fn harness_with_demand(demand: Arc<FakeDemand>) -> support::Harness {
    let config: multiview_config::MultiviewConfig =
        serde_json::from_value(base_document()).expect("fixture config");
    let seeded = multiview_control::seed_resources(&config).expect("seed");
    harness_with(move |state| {
        let state = state.with_source_demand(demand);
        state.with_seeded_resources(seeded)
    })
}

#[tokio::test]
async fn status_lists_every_source_and_idle_is_not_a_fault() {
    let h = harness_with_demand(Arc::new(FakeDemand::default()));
    let resp = send(&h.router, get("/api/v1/sources/status", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], "cam");
    assert_eq!(list[0]["phase"], "ready");
    assert_eq!(list[0]["activation"], "always_on");
    assert_eq!(list[0]["consumers"][0]["kind"], "layout_cell");
    assert_eq!(list[1]["id"], "idle");
    assert_eq!(list[1]["phase"], "idle");
    assert_eq!(list[1]["activation"], "on_demand");
    assert_eq!(list[1]["consumers"], json!([]));
}

#[tokio::test]
async fn one_source_status_and_unknown_is_404() {
    let h = harness_with_demand(Arc::new(FakeDemand::default()));
    let resp = send(
        &h.router,
        get("/api/v1/sources/idle/status", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["phase"], "idle");
    let resp = send(
        &h.router,
        get("/api/v1/sources/ghost/status", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_idle_source_reports_not_probed_instead_of_an_empty_inventory() {
    let h = harness_with_demand(Arc::new(FakeDemand::default()));
    let resp = send(
        &h.router,
        get("/api/v1/inputs/idle/streams", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/not-probed");
    // An unknown id stays a 404, never "not probed".
    let resp = send(
        &h.router,
        get("/api/v1/inputs/ghost/streams", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn polling_a_preview_is_a_consumer_and_says_starting() {
    let demand = Arc::new(FakeDemand::default());
    let h = harness_with_demand(Arc::clone(&demand));
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/idle.jpg", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.headers().get("x-multiview-source-state").unwrap(),
        "starting"
    );
    assert_eq!(*demand.touched.lock().unwrap(), vec!["idle".to_owned()]);
}

#[tokio::test]
async fn start_is_operator_audited_and_returns_the_state() {
    let demand = Arc::new(FakeDemand::default());
    let h = harness_with_demand(Arc::clone(&demand));
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/idle/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await["id"], "idle");
    assert_eq!(*demand.started.lock().unwrap(), vec!["idle".to_owned()]);
    let audit = body_json(send(&h.router, get("/api/v1/audit", ADMIN_TOKEN)).await).await;
    let newest = &audit.as_array().unwrap()[0];
    assert_eq!(newest["action"], "command");
    assert_eq!(newest["object_id"], "idle");
    assert_eq!(newest["detail"]["command"], "start");
}

#[tokio::test]
async fn start_and_stop_are_per_object_and_role_gated() {
    let demand = Arc::new(FakeDemand::default());
    let h = harness_with_demand(Arc::clone(&demand));
    for path in ["/api/v1/sources/idle/start", "/api/v1/sources/idle/stop"] {
        // A viewer cannot operate; an operator scoped to another object is
        // denied this one (BOLA).
        for token in [VIEWER_TOKEN, SCOPED_TOKEN] {
            let resp = send(&h.router, post_json(path, token, &json!({}))).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
        }
        let resp = send(
            &h.router,
            post_json(&path.replace("idle", "ghost"), OPERATOR_TOKEN, &json!({})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
    assert!(demand.started.lock().unwrap().is_empty());
    assert!(demand.stopped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stopping_a_source_in_use_is_refused_naming_its_consumers() {
    let demand = Arc::new(FakeDemand::default());
    let h = harness_with_demand(Arc::clone(&demand));
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/cam/stop", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let problem = body_json(resp).await;
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("layout_cell cell-1"),
        "{problem}"
    );
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/idle/stop", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(*demand.stopped.lock().unwrap(), vec!["idle".to_owned()]);
}
