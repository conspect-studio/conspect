//! Source event bridge authorization, exact identity and bounded wire contract.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use axum::http::StatusCode;
use multiview_control::source_events::{
    SourceEventAvailability, SourceEventProvider, SourceEventTestInjector, SourceEventsDto,
    TestEventBoundary, TestEventOutcome,
};
use std::sync::{Arc, Mutex};
mod support;
use support::{
    body_json, get, harness, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    SCOPED_TOKEN, VIEWER_TOKEN,
};

#[derive(Default)]
struct Spy {
    calls: Mutex<Vec<String>>,
}
impl SourceEventProvider for Spy {
    fn source_events(&self, id: &str) -> Option<SourceEventsDto> {
        self.calls.lock().unwrap().push(id.to_owned());
        (id != "unknown").then(|| {
            SourceEventsDto::new(
                id,
                9_007_199_254_740_993,
                SourceEventAvailability::Available,
                false,
                vec![],
            )
        })
    }
}
#[test]
fn snapshot_bounds_rows_and_preserves_exact_event_epochs() {
    use multiview_control::source_events::{
        SourceEventDto, SourceEventLifecycle, SourceEventOrigin, MAX_SOURCE_EVENTS,
    };
    let event = SourceEventDto {
        event_id: "break-1".into(),
        epoch: u64::MAX.to_string(),
        origin: SourceEventOrigin::Scte35,
        vendor: None,
        lifecycle: SourceEventLifecycle::Announced,
        timer: None,
        asset_id: None,
        correlation_id: None,
        label: None,
        marker_name: None,
        marker_type_id: None,
        marker_detail: None,
    };
    let dto = SourceEventsDto::new(
        "cam",
        u64::MAX,
        SourceEventAvailability::Stale,
        false,
        vec![event.clone(); MAX_SOURCE_EVENTS + 1],
    );
    let json = serde_json::to_value(dto).unwrap();
    assert_eq!(json["events"].as_array().unwrap().len(), MAX_SOURCE_EVENTS);
    assert_eq!(json["events"][0]["epoch"], "18446744073709551615");
    assert_eq!(json["degraded"], true);
    let mut oversized = event.clone();
    oversized.event_id = "x".repeat(257);
    let mut invalid_epoch = event;
    invalid_epoch.epoch = "01".into();
    let json = serde_json::to_value(SourceEventsDto::new(
        "cam",
        1,
        SourceEventAvailability::Unavailable,
        false,
        vec![oversized, invalid_epoch],
    ))
    .unwrap();
    assert_eq!(json["events"], serde_json::json!([]));
    assert_eq!(json["degraded"], true);
}

fn timed_event() -> multiview_control::source_events::SourceEventDto {
    use multiview_control::source_events::*;
    SourceEventDto {
        event_id: "occurrence".into(),
        epoch: u64::MAX.to_string(),
        origin: SourceEventOrigin::Scte35,
        vendor: None,
        lifecycle: SourceEventLifecycle::Uncertain,
        timer: Some(SourceEventTimer {
            kind: SourceEventTimerKind::Overrun,
            nanoseconds: u64::MAX.to_string(),
        }),
        asset_id: Some("asset".into()),
        correlation_id: Some("correlation".into()),
        label: Some(SourceEventLabel::Overrun),
        marker_name: Some("Break Start".into()),
        marker_type_id: Some("0x22".into()),
        marker_detail: Some("2/4".into()),
    }
}

#[test]
fn projected_timer_and_public_identity_are_exact_and_bounded() {
    let dto = SourceEventsDto::new(
        "cam",
        1,
        SourceEventAvailability::Available,
        false,
        vec![timed_event()],
    );
    let json = serde_json::to_value(dto).unwrap();
    assert_eq!(
        json["events"][0]["timer"]["nanoseconds"],
        "18446744073709551615"
    );
    assert_eq!(json["events"][0]["timer"]["kind"], "overrun");
    assert_eq!(json["events"][0]["label"], "overrun");
    assert_eq!(json["events"][0]["asset_id"], "asset");
    assert_eq!(json["events"][0]["correlation_id"], "correlation");
    assert_eq!(json["degraded"], false);
    for value in [
        "",
        "01",
        "-1",
        "+1",
        "1.5",
        "18446744073709551616",
        "000000000000000000000",
    ] {
        let mut row = timed_event();
        row.timer.as_mut().unwrap().nanoseconds = value.into();
        let json = serde_json::to_value(SourceEventsDto::new(
            "cam",
            1,
            SourceEventAvailability::Available,
            false,
            vec![row],
        ))
        .unwrap();
        assert_eq!(json["events"], serde_json::json!([]));
        assert_eq!(json["degraded"], true);
    }
    for asset in [true, false] {
        let mut row = timed_event();
        if asset {
            row.asset_id = Some("é".repeat(129));
        } else {
            row.correlation_id = Some("x".repeat(257));
        }
        let json = serde_json::to_value(SourceEventsDto::new(
            "cam",
            1,
            SourceEventAvailability::Available,
            false,
            vec![row],
        ))
        .unwrap();
        assert_eq!(json["events"], serde_json::json!([]));
        assert_eq!(json["degraded"], true);
    }
}

#[test]
fn stale_clocks_and_contradictory_semantics_cannot_supply_timers() {
    use multiview_control::source_events::*;
    for availability in [
        SourceEventAvailability::Stale,
        SourceEventAvailability::Unavailable,
    ] {
        let json = serde_json::to_value(SourceEventsDto::new(
            "cam",
            1,
            availability,
            false,
            vec![timed_event()],
        ))
        .unwrap();
        assert_eq!(json["events"], serde_json::json!([]));
        assert_eq!(json["degraded"], true);
    }
    let mut ended = timed_event();
    ended.lifecycle = SourceEventLifecycle::Ended;
    let mut vendor = timed_event();
    vendor.origin = SourceEventOrigin::VendorMarker;
    vendor.vendor = Some("vendor".into());
    let mut empty = timed_event();
    empty.event_id.clear();
    let json = serde_json::to_value(SourceEventsDto::new(
        "cam",
        1,
        SourceEventAvailability::Available,
        false,
        vec![ended, vendor, empty],
    ))
    .unwrap();
    assert_eq!(json["events"], serde_json::json!([]));
    assert_eq!(json["degraded"], true);
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_exposes_typed_authenticated_snapshot() {
    use utoipa::OpenApi;
    let spec = serde_json::to_value(multiview_control::openapi::ApiDoc::openapi()).unwrap();
    let operation = &spec["paths"]["/api/v1/inputs/{id}/events"]["get"];
    assert_eq!(
        operation["security"][0]["bearer_auth"],
        serde_json::json!([])
    );
    for status in ["200", "401", "403", "404", "503"] {
        assert!(operation["responses"][status].is_object());
    }
    let schema = &spec["components"]["schemas"]["SourceEventsDto"];
    assert_eq!(schema["properties"]["epoch"]["type"], "string");
    assert_eq!(schema["properties"]["events"]["maxItems"], 256);
}

#[tokio::test]
async fn authentication_and_scope_precede_provider_lookup() {
    let spy = Arc::new(Spy::default());
    let probe = spy.clone();
    let h = harness_with(|s| s.with_source_events(spy));
    for (token, id, status) in [
        ("", "cam", StatusCode::UNAUTHORIZED),
        (SCOPED_TOKEN, "cam", StatusCode::FORBIDDEN),
        (SCOPED_TOKEN, "scoped-layout.jpg", StatusCode::FORBIDDEN),
    ] {
        let response = send(
            &h.router,
            get(&format!("/api/v1/inputs/{id}/events"), token),
        )
        .await;
        assert_eq!(response.status(), status);
    }
    assert!(probe.calls.lock().unwrap().is_empty());
    assert_eq!(
        send(
            &h.router,
            get("/api/v1/inputs/scoped-layout/events", SCOPED_TOKEN)
        )
        .await
        .status(),
        StatusCode::OK
    );
}
#[tokio::test]
async fn unavailable_unknown_and_empty_are_distinct() {
    let h = harness();
    let response = send(&h.router, get("/api/v1/inputs/cam/events", VIEWER_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let h = harness_with(|s| s.with_source_events(Arc::new(Spy::default())));
    let unknown = send(
        &h.router,
        get("/api/v1/inputs/unknown/events", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown.headers()["cache-control"], "no-store");
    let response = send(
        &h.router,
        get("/api/v1/inputs/cam.jpg/events", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = body_json(response).await;
    assert_eq!(body["source_id"], "cam.jpg");
    assert_eq!(body["epoch"], "9007199254740993");
    assert_eq!(body["events"], serde_json::json!([]));
    assert_eq!(body["availability"], "available");
}

#[derive(Default)]
struct InjectorSpy {
    calls: Mutex<Vec<(String, TestEventBoundary)>>,
}
impl SourceEventTestInjector for InjectorSpy {
    fn inject_test_event(&self, id: &str, boundary: TestEventBoundary) -> TestEventOutcome {
        self.calls.lock().unwrap().push((id.to_owned(), boundary));
        match id {
            "unknown" => TestEventOutcome::Unknown,
            "full" => TestEventOutcome::Overflow,
            _ => TestEventOutcome::Queued,
        }
    }
}

#[tokio::test]
async fn test_injection_requires_administer_role_and_object_scope() {
    let spy = Arc::new(InjectorSpy::default());
    let h = harness_with(|s| s.with_source_event_test_injector(spy.clone()));
    let body = serde_json::json!({ "boundary": "start" });
    for (token, id, status) in [
        ("", "cam", StatusCode::UNAUTHORIZED),
        (VIEWER_TOKEN, "cam", StatusCode::FORBIDDEN),
        (OPERATOR_TOKEN, "cam", StatusCode::FORBIDDEN),
        (SCOPED_TOKEN, "cam", StatusCode::FORBIDDEN),
    ] {
        let response = send(
            &h.router,
            post_json(&format!("/api/v1/inputs/{id}/events/test"), token, &body),
        )
        .await;
        assert_eq!(response.status(), status, "token {token:?} id {id:?}");
    }
    assert!(spy.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_injection_is_unavailable_without_an_adapter() {
    let h = harness();
    let response = send(
        &h.router,
        post_json(
            "/api/v1/inputs/cam/events/test",
            ADMIN_TOKEN,
            &serde_json::json!({ "boundary": "start" }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_injection_distinguishes_queued_unknown_and_overflow() {
    let spy = Arc::new(InjectorSpy::default());
    let h = harness_with(|s| s.with_source_event_test_injector(spy.clone()));
    let queued = send(
        &h.router,
        post_json(
            "/api/v1/inputs/cam/events/test",
            ADMIN_TOKEN,
            &serde_json::json!({ "boundary": "start", "duration_ns": 30_000_000_000_i64 }),
        ),
    )
    .await;
    assert_eq!(queued.status(), StatusCode::ACCEPTED);
    let end = send(
        &h.router,
        post_json(
            "/api/v1/inputs/cam/events/test",
            ADMIN_TOKEN,
            &serde_json::json!({ "boundary": "end" }),
        ),
    )
    .await;
    assert_eq!(end.status(), StatusCode::ACCEPTED);
    let unknown = send(
        &h.router,
        post_json(
            "/api/v1/inputs/unknown/events/test",
            ADMIN_TOKEN,
            &serde_json::json!({ "boundary": "start" }),
        ),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let full = send(
        &h.router,
        post_json(
            "/api/v1/inputs/full/events/test",
            ADMIN_TOKEN,
            &serde_json::json!({ "boundary": "start" }),
        ),
    )
    .await;
    assert_eq!(full.status(), StatusCode::SERVICE_UNAVAILABLE);
    let calls = spy.calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].0, "cam");
    assert_eq!(
        calls[0].1,
        TestEventBoundary::Start {
            duration_ns: Some(30_000_000_000)
        }
    );
    assert_eq!(calls[1].1, TestEventBoundary::End);
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_exposes_the_test_injection_route() {
    use utoipa::OpenApi;
    let spec = serde_json::to_value(multiview_control::openapi::ApiDoc::openapi()).unwrap();
    let operation = &spec["paths"]["/api/v1/inputs/{id}/events/test"]["post"];
    assert_eq!(
        operation["security"][0]["bearer_auth"],
        serde_json::json!([])
    );
    for status in ["202", "401", "403", "404", "503"] {
        assert!(operation["responses"][status].is_object());
    }
}
