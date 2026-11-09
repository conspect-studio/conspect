#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_control::routes::switcher::{authorize_refs, SwitcherRequest};
use multiview_control::{
    EngineStateSnapshot, SharedSourceDemand, SourceDemandLease, SourceDemandPhase,
    SourceDemandProvider, SourceDemandView,
};
use multiview_control::{Principal, Role};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{ScenePair, SwitcherControl};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
fn primed_stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["a", "b"]
        .into_iter()
        .map(|id| {
            let store = Arc::new(TileStore::new(id, thresholds, NoSignalPolicy::Slate));
            store.publish(
                Nv12Image::solid(
                    64,
                    64,
                    128,
                    128,
                    128,
                    ColorInfo::default().resolve_defaults(64, 64),
                )
                .unwrap(),
                MediaTime::ZERO,
            );
            (id.to_owned(), store)
        })
        .collect()
}
use support::{body_json, get, harness, harness_with, post_json, send, OPERATOR_TOKEN};

fn request(action: &str, source: Option<&str>) -> serde_json::Value {
    json!({ "me": "me1", "program": "main", "action": action, "source": source })
}
fn scene(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: id.into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            source: Some(id.into()),
            ..Cell::default()
        }],
    })
}
fn runtime() -> (
    SwitcherControl,
    multiview_engine::switcher_control::SwitcherTickDriver,
) {
    let (handle, driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    // Only a real frame publisher may make the REST capability live. Tests
    // emulating its tick retain the driver so the completion can be verified.
    let mut driver = driver;
    driver.set_prime_stores(primed_stores());
    driver.publish_provider_status(multiview_engine::switcher_control::ProviderStatus {
        scene_version: 1,
        last_tick: 0,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    (handle, driver)
}
// reason: the helper mirrors the route's own `Json<SwitcherRequest>` extraction
// and hands the owned body to `post_json`, which borrows it.
#[allow(clippy::needless_pass_by_value)]
fn post_with_key(path: &str, token: &str, body: serde_json::Value, key: &str) -> Request<Body> {
    let mut request = post_json(path, token, &body);
    request
        .headers_mut()
        .insert("Idempotency-Key", key.parse().unwrap());
    request
}

#[tokio::test]
async fn unwired_routes_never_report_applied_or_accept_a_command() {
    let h = harness();
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "cut-unwired",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem = body_json(response).await;
    assert!(problem["detail"]
        .as_str()
        .unwrap()
        .contains("no real Program/Preview"));
}

#[tokio::test]
async fn real_handle_returns_202_then_tick_exact_correlated_completion() {
    let (handle, mut driver) = runtime();
    let mut events = handle.outcomes();
    let h = harness_with(|state| state.with_switcher(handle));
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "cut-one",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = body_json(response).await;
    let op = accepted["operation_id"].as_str().unwrap();
    assert!(!accepted.to_string().contains("applied"));
    assert!(
        events.try_recv().is_err(),
        "202 is not a claim of on-air completion"
    );
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "cut-one",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(response).await["operation_id"], op);
    driver.drain_at_tick(Tick {
        index: 9,
        pts: MediaTime::ZERO,
    });
    driver.commit_composed_tick(Tick {
        index: 9,
        pts: MediaTime::ZERO,
    });
    let outcome = events.try_recv().unwrap();
    assert_eq!(outcome.event.operation_id, op);
    assert_eq!(outcome.event.tick, 9);
    assert!(outcome.event.completed);
    assert!(
        events.try_recv().is_err(),
        "idempotent replay never queues a second cut"
    );
}

#[tokio::test]
async fn full_idempotency_store_rejects_switcher_command_without_enqueue() {
    use multiview_control::concurrency::{Reservation, MAX_IDEMPOTENCY_RESERVATIONS};

    let (handle, mut driver) = runtime();
    let mut events = handle.outcomes();
    let h = harness_with(|state| {
        for index in 0..MAX_IDEMPOTENCY_RESERVATIONS {
            assert!(matches!(
                state
                    .idempotency
                    .reserve(Some(&format!("occupied-{index}"))),
                Reservation::Fresh(_)
            ));
        }
        state.with_switcher(handle)
    });
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/cut",
            OPERATOR_TOKEN,
            request("cut", None),
            "capacity-cut",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    driver.drain_at_tick(Tick {
        index: 9,
        pts: MediaTime::ZERO,
    });
    driver.commit_composed_tick(Tick {
        index: 9,
        pts: MediaTime::ZERO,
    });
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn reference_bola_checks_me_source_layout_output_and_program_before_enqueue() {
    let (handle, _driver) = runtime();
    let cases = vec![
        (
            vec!["me1", "a", "layout-a"],
            vec!["program:main", "out-a"],
            "me2",
            None,
            None,
            None,
        ),
        (
            vec!["me1", "layout-a"],
            vec!["program:main", "out-a"],
            "me1",
            Some("a"),
            None,
            None,
        ),
        (
            vec!["me1", "a"],
            vec!["program:main", "out-a"],
            "me1",
            Some("a"),
            Some("layout-a"),
            None,
        ),
        (
            vec!["me1", "a", "layout-a"],
            vec!["program:main"],
            "me1",
            Some("a"),
            Some("layout-a"),
            Some("out-a"),
        ),
        (
            vec!["me1", "a", "layout-a"],
            vec!["out-a"],
            "me1",
            Some("a"),
            Some("layout-a"),
            Some("out-a"),
        ),
    ];
    for (objects, outputs, me, source, layout, output) in cases {
        let principal = Principal {
            key_id: "case".into(),
            role: Role::Operator,
            scoped_object_ids: Some(objects.into_iter().map(str::to_owned).collect()),
            scoped_output_ids: Some(outputs.into_iter().map(str::to_owned).collect()),
            scoped_discovery_domains: None,
        };
        let body = json!({"me":me,"program":"main","action":"select_preview","source":source,"layout":layout,"output":output});
        // The route's BOLA function runs before unsupported-slice validation.
        let req: SwitcherRequest = serde_json::from_value(body).unwrap();
        assert!(matches!(
            authorize_refs(&principal, &req),
            Err(multiview_control::ControlError::Forbidden(_))
        ));
    }
    drop(handle);
}

// ---------------------------------------------------------------------------
// Sustained PVW leases: make-before-break, honest rejection, auth-before-lease
// ---------------------------------------------------------------------------

/// Records lease acquisitions and drops, so "the replacement was acquired before
/// the previous lease was released" and "a rejected take keeps the previous PVW
/// lease" are both directly observable.
#[derive(Default)]
struct LeaseLog {
    acquired: std::sync::Mutex<Vec<String>>,
    dropped: std::sync::Mutex<Vec<String>>,
}

impl LeaseLog {
    fn acquired(&self) -> Vec<String> {
        self.acquired.lock().unwrap().clone()
    }
    fn dropped(&self) -> Vec<String> {
        self.dropped.lock().unwrap().clone()
    }
}

struct TaggedLease {
    tag: String,
    log: Arc<LeaseLog>,
}

impl SourceDemandLease for TaggedLease {}

impl Drop for TaggedLease {
    fn drop(&mut self) {
        self.log.dropped.lock().unwrap().push(self.tag.clone());
    }
}

/// A demand provider handing out tagged leases for the allow-listed ids only —
/// the stub stands in for the run's real on-demand manager.
struct LeaseDemand {
    log: Arc<LeaseLog>,
}

impl SourceDemandProvider for LeaseDemand {
    fn status(&self, id: &str) -> SourceDemandView {
        SourceDemandView::idle(id)
    }
    fn active(&self) -> Vec<SourceDemandView> {
        Vec::new()
    }
    fn preview_lease(&self, id: &str, _label: &str) -> Option<Box<dyn SourceDemandLease>> {
        if !matches!(id, "a" | "b") {
            return None;
        }
        self.log.acquired.lock().unwrap().push(id.to_owned());
        Some(Box::new(TaggedLease {
            tag: id.to_owned(),
            log: Arc::clone(&self.log),
        }))
    }
    fn touch_preview(&self, _id: &str) -> Option<SourceDemandPhase> {
        None
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn preview_request(source: &str) -> serde_json::Value {
    request("select_preview", Some(source))
}

/// The lease owner is make-before-break: an accepted PVW selection holds its new
/// lease while the previous one stays alive, a terminal failure drops only the
/// new lease (the old PVW demand is never lost), and an applied selection
/// releases the old lease only after the engine receipt — all resolved through
/// the real outcome relay, not by reaching into the owner.
#[tokio::test]
#[allow(clippy::too_many_lines)]
// reason: one straight-line proof of the two-phase lease contract (admit, hold
// across the old lease, reject, apply, replace) — splitting it would break the
// single narrative the observable drop order depends on.
async fn preview_lease_replacement_is_make_before_break_and_a_rejection_keeps_the_old_pvw() {
    let (handle, mut driver) = runtime();
    let log = Arc::new(LeaseLog::default());
    let leases = Arc::new(multiview_control::source_demand::SwitcherPreviewLeases::default());
    leases.seed(multiview_control::source_demand::SwitcherPreviewLease::new(
        Box::new(TaggedLease {
            tag: "boot".into(),
            log: Arc::clone(&log),
        }),
    ));
    let demand: SharedSourceDemand = Arc::new(LeaseDemand {
        log: Arc::clone(&log),
    });
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    tokio::spawn(
        multiview_control::switcher_events::forward_switcher_outcomes(
            handle.outcomes(),
            Arc::clone(&publisher),
            "me1".into(),
            "main".into(),
            Arc::clone(&leases),
        ),
    );
    let handle_for_state = handle.clone();
    let leases_for_state = Arc::clone(&leases);
    let h = harness_with(move |state| {
        let mut state = state
            .with_switcher(handle_for_state)
            .with_source_demand(demand);
        state.switcher_preview_lease = leases_for_state;
        state
    });

    // 1. An accepted selection acquires its lease without touching the live one.
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            preview_request("a"),
            "pv-a",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let op_a = body_json(response).await["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(log.acquired(), vec!["a".to_owned()]);
    assert!(
        log.dropped().is_empty(),
        "the boot lease must survive admission of its replacement"
    );
    assert_eq!(leases.pending_count(), 1);

    // 2. An idempotent replay never re-leases.
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            preview_request("a"),
            "pv-a",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(response).await["operation_id"], op_a);
    assert_eq!(log.acquired(), vec!["a".to_owned()]);

    // 3. The engine's applied receipt releases the boot lease (old after new).
    driver.drain_at_tick(Tick {
        index: 1,
        pts: MediaTime::ZERO,
    });
    driver.commit_composed_tick(Tick {
        index: 1,
        pts: MediaTime::ZERO,
    });
    until("the applied selection releases the boot lease", || {
        log.dropped() == vec!["boot".to_owned()]
    })
    .await;
    assert_eq!(leases.pending_count(), 0);

    // 4. The next selection is held, and the now-live `a` lease is still held:
    //    acquired-before-released.
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            preview_request("b"),
            "pv-b",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(log.acquired(), vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(
        log.dropped(),
        vec!["boot".to_owned()],
        "the live `a` lease must still be held while its replacement is pending"
    );
    assert_eq!(leases.pending_count(), 1);

    // 5. A re-authored scene generation between acceptance and the frame
    //    boundary makes the engine reject the queued selection: only the new,
    //    never-applied lease is dropped.
    handle
        .set_scenes(ScenePair {
            version: 2,
            program: scene("a"),
            preview: scene("b"),
        })
        .unwrap();
    driver.drain_at_tick(Tick {
        index: 2,
        pts: MediaTime::ZERO,
    });
    until("the rejected selection drops only its own lease", || {
        log.dropped() == vec!["boot".to_owned(), "b".to_owned()]
    })
    .await;
    assert_eq!(leases.pending_count(), 0);
    // The next fresh frame republishes capability for the new generation, as the
    // engine hook does — never before it.
    driver.publish_provider_status(multiview_engine::switcher_control::ProviderStatus {
        scene_version: 2,
        last_tick: 2,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });

    // 6. The rejected selection never changed the effective PVW bus.
    let response = send(&h.router, get("/api/v1/switcher/me1", OPERATOR_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["preview_source"], "a");

    // 7. A later selection that IS applied releases `a` only after its receipt.
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            preview_request("b"),
            "pv-b2",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        log.dropped(),
        vec!["boot".to_owned(), "b".to_owned()],
        "the pending lease replaces the live one only on the applied receipt"
    );
    driver.drain_at_tick(Tick {
        index: 3,
        pts: MediaTime::ZERO,
    });
    driver.commit_composed_tick(Tick {
        index: 3,
        pts: MediaTime::ZERO,
    });
    until(
        "the applied replacement releases the previous live lease",
        || log.dropped() == vec!["boot".to_owned(), "b".to_owned(), "a".to_owned()],
    )
    .await;
    assert_eq!(leases.pending_count(), 0);
}

/// Every referenced resource is authorized BEFORE any demand is touched: a
/// principal scoped to the M/E, the program and one source is denied a second
/// source, and that denial opens no ingest and holds no lease.
#[tokio::test]
async fn denied_reference_never_acquires_a_preview_lease_or_opens_a_source() {
    let (handle, _driver) = runtime();
    let log = Arc::new(LeaseLog::default());
    let leases = Arc::new(multiview_control::source_demand::SwitcherPreviewLeases::default());
    let demand: SharedSourceDemand = Arc::new(LeaseDemand {
        log: Arc::clone(&log),
    });
    let leases_for_state = Arc::clone(&leases);
    let h = harness_with(move |state| {
        let mut state = state.with_switcher(handle).with_source_demand(demand);
        state.switcher_preview_lease = leases_for_state;
        Arc::get_mut(&mut state.api_keys)
            .expect("the harness key store is uniquely owned before the router is built")
            .register(
                "me-scoped-key",
                "me-scoped-secret-x1",
                Principal {
                    key_id: "me-scoped-key".into(),
                    role: Role::Operator,
                    scoped_object_ids: Some(["me1", "a"].into_iter().map(str::to_owned).collect()),
                    scoped_output_ids: Some(vec!["program:main".to_owned()]),
                    scoped_discovery_domains: None,
                },
            );
        state
    });
    let token = "me-scoped-key.me-scoped-secret-x1";
    // `b` is a real configured source but out of this principal's scope; `zzz`
    // is both out of scope and unknown. Both must fail closed before any lease.
    for source in ["b", "zzz"] {
        let response = send(
            &h.router,
            post_with_key(
                "/api/v1/switcher/commands/select_preview",
                token,
                preview_request(source),
                &format!("denied-{source}"),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{source}");
    }
    assert!(
        log.acquired().is_empty(),
        "a denied reference must not open any source"
    );
    assert_eq!(leases.pending_count(), 0);
}
