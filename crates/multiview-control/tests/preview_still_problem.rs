//! The input still route's typed `503` (`GET /api/v1/preview/inputs/{id}.jpg`).
//!
//! A still with no picture must say why: an RFC 9457 problem whose stable `type`
//! names the source's demand phase (`source-failed`, `source-starting`,
//! `preview-not-served`) and, for a failed source, carries the engine's bounded
//! operator-safe reason. These tests also pin the BOLA guarantee this route
//! already had (SEC-05 / ADR-W005): per-object authorization runs BEFORE any
//! phase is read, so a principal without access to the source gets exactly the
//! same refusal as before and learns nothing about its state.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::{Arc, Mutex};

use axum::http::{header, StatusCode};
use multiview_control::{
    PreviewProvider, SourceDemandLease, SourceDemandPhase, SourceDemandProvider, SourceDemandView,
    StopRefusal,
};
use support::{body_json, get, harness_with, send, OPERATOR_TOKEN, SCOPED_TOKEN};

/// The measured defect: the provider has no frame for any input.
struct NoFramePreview;

impl PreviewProvider for NoFramePreview {
    fn program_jpeg(&self, _quality: u8) -> Option<Vec<u8>> {
        None
    }

    fn input_jpeg(&self, _id: &str, _quality: u8) -> Option<Vec<u8>> {
        None
    }

    fn input_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A provider that still holds a frame for one input (the last-good frame).
struct FramePreview(&'static str);

impl PreviewProvider for FramePreview {
    fn program_jpeg(&self, _quality: u8) -> Option<Vec<u8>> {
        None
    }

    fn input_jpeg(&self, id: &str, _quality: u8) -> Option<Vec<u8>> {
        (id == self.0).then(|| vec![0xff, 0xd8, 0xff])
    }

    fn input_ids(&self) -> Vec<String> {
        vec![self.0.to_owned()]
    }
}

/// A demand manager reporting one fixed phase, recording every preview touch.
struct PhaseDemand {
    phase: SourceDemandPhase,
    detail: Option<String>,
    touched: Mutex<Vec<String>>,
}

impl PhaseDemand {
    fn new(phase: SourceDemandPhase, detail: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            phase,
            detail: detail.map(str::to_owned),
            touched: Mutex::new(Vec::new()),
        })
    }
}

impl SourceDemandProvider for PhaseDemand {
    fn status(&self, id: &str) -> SourceDemandView {
        SourceDemandView {
            phase: self.phase,
            detail: self.detail.clone(),
            ..SourceDemandView::idle(id)
        }
    }

    fn active(&self) -> Vec<SourceDemandView> {
        vec![self.status("cam-1")]
    }

    fn preview_lease(&self, _id: &str, _label: &str) -> Option<Box<dyn SourceDemandLease>> {
        None
    }

    fn touch_preview(&self, id: &str) -> Option<SourceDemandPhase> {
        self.touched.lock().unwrap().push(id.to_owned());
        Some(self.phase)
    }

    fn start_source(&self, _id: &str) -> bool {
        false
    }

    fn stop_source(&self, _id: &str) -> Result<(), StopRefusal> {
        Ok(())
    }
}

fn assert_problem_is_503(content_type: Option<&str>) {
    assert_eq!(
        content_type,
        Some("application/problem+json"),
        "a refused still says why as an RFC 9457 problem"
    );
}

#[tokio::test]
async fn a_failed_source_still_is_a_typed_problem_with_its_reason() {
    let demand = PhaseDemand::new(
        SourceDemandPhase::Failed,
        Some("no media yet; still retrying"),
    );
    let probe = Arc::clone(&demand);
    let h = harness_with(move |state| {
        state
            .with_preview(Arc::new(NoFramePreview))
            .with_source_demand(demand)
    });

    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_problem_is_503(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    );
    // The coarse header keeps its old contract for existing clients.
    assert_eq!(
        resp.headers().get("x-multiview-source-state").unwrap(),
        "failed"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/source-failed");
    assert_eq!(problem["status"], 503);
    assert_eq!(
        problem["detail"], "no media yet; still retrying",
        "the operator sees the engine's reason, not a generic failure"
    );
    assert_eq!(*probe.touched.lock().unwrap(), vec!["cam-1".to_owned()]);
}

#[tokio::test]
async fn starting_and_idle_sources_are_typed_starting_and_leak_no_detail() {
    for phase in [SourceDemandPhase::Starting, SourceDemandPhase::Idle] {
        // A detail belongs to a failure; it must never ride a starting problem.
        let demand = PhaseDemand::new(phase, Some("internal reason"));
        let h = harness_with(move |state| {
            state
                .with_preview(Arc::new(NoFramePreview))
                .with_source_demand(demand)
        });
        let resp = send(
            &h.router,
            get("/api/v1/preview/inputs/cam-1.jpg", OPERATOR_TOKEN),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get("x-multiview-source-state").unwrap(),
            "starting"
        );
        let problem = body_json(resp).await;
        assert_eq!(problem["type"], "/problems/source-starting");
        assert_eq!(problem["status"], 503);
        assert!(
            problem.get("detail").is_none(),
            "a starting problem carries no source detail: {problem}"
        );
    }
}

#[tokio::test]
async fn a_ready_source_without_a_frame_is_not_served() {
    let demand = PhaseDemand::new(SourceDemandPhase::Ready, None);
    let h = harness_with(move |state| {
        state
            .with_preview(Arc::new(NoFramePreview))
            .with_source_demand(demand)
    });
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.headers().get("x-multiview-source-state").unwrap(),
        "ready"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/preview-not-served");
}

#[tokio::test]
async fn an_unknown_input_without_a_demand_manager_is_not_served_and_leaks_no_phase() {
    let h = harness_with(move |state| state.with_preview(Arc::new(NoFramePreview)));
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/ghost.jpg", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        resp.headers().get("x-multiview-source-state").is_none(),
        "no demand manager means no phase to state"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/preview-not-served");
}

#[tokio::test]
async fn a_failed_sources_detail_is_bounded_and_flattened() {
    let long = format!("line one\nline two\u{7}{}", "x".repeat(400));
    let demand = PhaseDemand::new(SourceDemandPhase::Failed, Some(&long));
    let h = harness_with(move |state| {
        state
            .with_preview(Arc::new(NoFramePreview))
            .with_source_demand(demand)
    });
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", OPERATOR_TOKEN),
    )
    .await;
    let problem = body_json(resp).await;
    let detail = problem["detail"].as_str().expect("detail is a string");
    assert!(
        detail.chars().count() <= 161,
        "detail is capped (160 chars + an ellipsis); got {} chars",
        detail.chars().count()
    );
    assert!(!detail.contains('\n'), "detail is single-line: {detail:?}");
    assert!(
        !detail.chars().any(char::is_control),
        "detail carries no control characters: {detail:?}"
    );
    assert!(
        detail.ends_with('…'),
        "a truncated detail says so: {detail:?}"
    );
    assert!(detail.starts_with("line one line two"));
}

#[tokio::test]
async fn an_unauthorized_principal_gets_the_same_refusal_and_no_phase() {
    let demand = PhaseDemand::new(
        SourceDemandPhase::Failed,
        Some("no media yet; still retrying"),
    );
    let probe = Arc::clone(&demand);
    let h = harness_with(move |state| {
        state
            .with_preview(Arc::new(NoFramePreview))
            .with_source_demand(demand)
    });

    // SCOPED_TOKEN is an operator scoped to another object, so cam-1 is out of
    // scope: the same 403 as before this change, with no phase anywhere.
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        resp.headers().get("x-multiview-source-state").is_none(),
        "a refusal carries no phase header"
    );
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    let body = serde_json::to_string(&problem).unwrap();
    for leaked in ["failed", "starting", "ready", "no media", "phase"] {
        assert!(
            !body.contains(leaked),
            "a BOLA refusal leaks nothing about the source: {body}"
        );
    }
    assert!(
        probe.touched.lock().unwrap().is_empty(),
        "authorization runs before the demand provider is consulted"
    );
}

#[tokio::test]
async fn a_source_holding_a_frame_still_serves_it_whatever_its_phase_says() {
    let demand = PhaseDemand::new(SourceDemandPhase::Failed, Some("no media yet"));
    let h = harness_with(move |state| {
        state
            .with_preview(Arc::new(FramePreview("cam-1")))
            .with_source_demand(demand)
    });
    let resp = send(
        &h.router,
        get("/api/v1/preview/inputs/cam-1.jpg", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a held last-good frame is served even while the source is failing"
    );
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
}
