//! Targeted chaos gates for the first-slice switcher's sustained PVW lease.
//!
//! Short, deterministic fault proofs (no soak): a bounded pending-selection
//! storm, a **dead correlated relay**, a scoped generation cancel, and the
//! off-clock sweep loop that reconciles all three against the engine's own
//! applied snapshot. Every vector here can lose a preview demand forever if the
//! lease owner is wrong, and every one runs in milliseconds.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use multiview_compositor::pipeline::Nv12Image;
use multiview_control::source_demand::{
    SwitcherLeaseSweep, SwitcherPreviewLease, SwitcherPreviewLeases,
};
use multiview_control::{
    SharedSourceDemand, SourceDemandLease, SourceDemandPhase, SourceDemandProvider,
    SourceDemandView,
};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::MediaTime;
use multiview_engine::clock::Tick;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ProviderStatus, ScenePair, SwitcherControl, SwitcherTickDriver,
};
use multiview_engine::StopSignal;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{harness_with, post_json, send, OPERATOR_TOKEN};

// ---------------------------------------------------------------------------
// Fixtures: a real engine handle, a lease-tagging demand stub, request helpers
// ---------------------------------------------------------------------------

/// Records lease acquisitions and drops, so "acquired before released" and "a
/// cancelled selection never resurrects" are directly observable.
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

impl TaggedLease {
    fn new(tag: &str, log: Arc<LeaseLog>) -> Self {
        Self {
            tag: tag.to_owned(),
            log,
        }
    }
}

impl SourceDemandLease for TaggedLease {}

impl Drop for TaggedLease {
    fn drop(&mut self) {
        self.log.dropped.lock().unwrap().push(self.tag.clone());
    }
}

/// A permissive demand provider: every configured id is rentable, so the tests
/// exercise the *engine's* admission and the lease owner's own bounds rather
/// than the stub's allow-list.
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
        self.log.acquired.lock().unwrap().push(id.to_owned());
        Some(Box::new(TaggedLease::new(id, Arc::clone(&self.log))))
    }
    fn touch_preview(&self, _id: &str) -> Option<SourceDemandPhase> {
        None
    }
}

/// The running configuration's sources (ADR-0104 §1: a switcher command may
/// reference only a configured source the principal may read).
fn configured_sources(ids: &[&str]) -> Arc<dyn multiview_control::ResourceRepository> {
    let store = multiview_control::InMemorySourceStore::new();
    for id in ids {
        multiview_control::ResourceRepository::create(
            &store,
            id,
            multiview_control::ResourceInput {
                name: (*id).to_owned(),
                body: json!({ "id": id, "kind": "solid" }),
            },
        )
        .unwrap();
    }
    Arc::new(store)
}

fn scene(id: &str, sources: &[&str]) -> Arc<Layout> {
    Arc::new(Layout {
        name: id.into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: sources
            .iter()
            .map(|source| Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                source: Some((*source).to_owned()),
                ..Cell::default()
            })
            .collect(),
    })
}

fn primed_stores() -> HashMap<String, Arc<TileStore<Nv12Image>>> {
    let thresholds = TileThresholds::from_millis(500, 2_000, 10_000).unwrap();
    ["a", "b", "d"]
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

fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::from_nanos(i64::try_from(index).unwrap() * 40_000_000),
    }
}

/// A live switcher handle that honestly reports capability (a real frame was
/// published), as the engine hook does.
fn runtime() -> (SwitcherControl, SwitcherTickDriver) {
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: scene("program", &["a"]),
            preview: scene("preview", &["b"]),
        })
        .unwrap();
    driver.set_prime_stores(primed_stores());
    driver.publish_provider_status(ProviderStatus {
        scene_version: 1,
        last_tick: 0,
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    (handle, driver)
}

fn request(action: &str, source: Option<&str>) -> serde_json::Value {
    json!({ "me": "me1", "program": "main", "action": action, "source": source })
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

/// Advance the engine by `count` composed ticks (a drained command is applied,
/// published, and its applied tick advances — the sweeper's only authority).
fn advance(driver: &mut SwitcherTickDriver, from: u64, count: u64) {
    for offset in 0..count {
        let next = tick(from + offset);
        driver.drain_at_tick(next);
        driver.commit_composed_tick(next);
    }
}

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------

/// The accept path is bounded: a storm fills exactly `MAX_PENDING`, the next
/// request is shed (releasing its own fresh lease immediately), and one bounded
/// off-clock sweep drains the whole table without ever cancelling the engine's
/// live, on-air lease.
#[tokio::test]
async fn pending_storm_is_bounded_and_one_sweep_drains_it_without_losing_the_live_lease() {
    let (handle, mut driver) = runtime();
    let observed = handle.clone();
    let log = Arc::new(LeaseLog::default());
    let leases = Arc::new(SwitcherPreviewLeases::default());
    leases.seed(SwitcherPreviewLease::new(Box::new(TaggedLease::new(
        "boot",
        Arc::clone(&log),
    ))));
    let h = harness_with({
        let log = Arc::clone(&log);
        let leases = Arc::clone(&leases);
        move |state| {
            let demand: SharedSourceDemand = Arc::new(LeaseDemand { log });
            let mut state = state
                .with_sources_store(configured_sources(&["a", "b", "c", "d", "e"]))
                .with_switcher(handle)
                .with_source_demand(demand);
            state.switcher_preview_lease = leases;
            state
        }
    });

    for index in 0..SwitcherPreviewLeases::MAX_PENDING {
        let response = send(
            &h.router,
            post_with_key(
                "/api/v1/switcher/commands/select_preview",
                OPERATOR_TOKEN,
                request("select_preview", Some("b")),
                &format!("storm-{index}"),
            ),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "storm entry {index}"
        );
    }
    assert_eq!(leases.pending_count(), SwitcherPreviewLeases::MAX_PENDING);
    assert_eq!(
        log.acquired().len(),
        SwitcherPreviewLeases::MAX_PENDING,
        "one acquisition per accepted storm entry"
    );
    assert!(
        log.dropped().is_empty(),
        "every accepted storm entry still holds its lease while the engine decides"
    );

    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            request("select_preview", Some("b")),
            "storm-overflow",
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the 65th selection is shed at the edge"
    );
    assert_eq!(
        leases.pending_count(),
        SwitcherPreviewLeases::MAX_PENDING,
        "a refused selection never inflates the table"
    );
    assert_eq!(
        log.dropped().len(),
        1,
        "the refused request releases its own lease immediately"
    );

    // Drain the real FIFO, then reconcile from its actual committed picture.
    advance(&mut driver, 0, 8);
    let report = leases.sweep(
        Some(observed.snapshot().unwrap().preview.as_str()),
        observed.composed_count(),
        Instant::now() + Duration::from_secs(5),
        SwitcherPreviewLeases::MAX_PENDING,
    );
    assert_eq!(report.dropped, 0);
    assert_eq!(report.adopted, SwitcherPreviewLeases::MAX_PENDING);
    assert_eq!(report.retained, 0);
    assert_eq!(leases.pending_count(), 0);
    assert!(leases.has_current());
    assert!(
        log.dropped().iter().any(|tag| tag == "boot"),
        "the adopted live demand replaces the boot lease"
    );
    assert_eq!(
        leases.sweep(
            Some("a"),
            u64::MAX,
            Instant::now() + Duration::from_secs(10),
            64
        ),
        SwitcherLeaseSweep::default(),
        "a drained table makes the next pass a no-op"
    );
}

/// A dead correlated relay must not leak a preview demand forever — and must
/// not cancel a selection the engine actually applied. The sweep reconciles
/// against the engine's own applied snapshot: an applied-but-unaware selection
/// is adopted, an obsolete resource reservation is released without inferring its outcome, and a dropped one can never air later.
#[tokio::test]
#[allow(clippy::too_many_lines)]
// reason: one straight-line fault walk (applied-but-unaware, then rejected-but-
// unaware, then no-later-air) whose ordering is the proof; splitting it would
// hide that the second phase starts from the first phase's live lease.
async fn a_dead_relay_still_reconciles_applied_and_rejected_selections() {
    let (handle, mut driver) = runtime();
    let log = Arc::new(LeaseLog::default());
    let leases = Arc::new(SwitcherPreviewLeases::default());
    leases.seed(SwitcherPreviewLease::new(Box::new(TaggedLease::new(
        "boot",
        Arc::clone(&log),
    ))));
    let engine = handle.clone();
    let h = harness_with({
        let log = Arc::clone(&log);
        let leases = Arc::clone(&leases);
        move |state| {
            let demand: SharedSourceDemand = Arc::new(LeaseDemand { log });
            let mut state = state
                .with_sources_store(configured_sources(&["a", "b", "c", "d", "e"]))
                .with_switcher(engine.clone())
                .with_source_demand(demand);
            state.switcher_preview_lease = leases;
            state
        }
    });
    let relay = tokio::spawn(
        multiview_control::switcher_events::forward_switcher_outcomes(
            handle.outcomes(),
            Arc::new(multiview_engine::isolation::EnginePublisher::<
                multiview_control::state::EngineStateSnapshot,
                multiview_events::Event,
            >::new(64)),
            "me1".into(),
            "main".into(),
            Arc::clone(&leases),
        ),
    );
    relay.abort();
    assert!(relay.await.unwrap_err().is_cancelled());
    advance(&mut driver, 0, 3);

    // (a) APPLIED but unaware: the engine really arms `a`; nobody hears it.
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            request("select_preview", Some("d")),
            "applied-unaware",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    advance(&mut driver, 3, 8);
    assert_eq!(
        handle.snapshot().unwrap().preview.as_str(),
        "d",
        "the engine applied the selection"
    );
    assert_eq!(
        leases.pending_count(),
        1,
        "with the relay dead no outcome ever resolves it"
    );
    let report = leases.sweep(
        Some(handle.snapshot().unwrap().preview.as_str()),
        handle.composed_count(),
        Instant::now() + Duration::from_secs(5),
        64,
    );
    assert_eq!(
        report.adopted, 1,
        "an applied selection is adopted, not cancelled"
    );
    assert!(leases.has_current());
    assert!(
        log.dropped().iter().any(|tag| tag == "boot"),
        "adopting the replacement released the previous live lease (make-before-break)"
    );
    assert_eq!(leases.pending_count(), 0);

    // (b) REJECTED but unaware: a windowed scene generation no longer admits a
    // source that is not authored into the preview scene.
    handle
        .set_scenes(ScenePair {
            version: 2,
            program: scene("program", &["a", "b"]),
            preview: scene("preview", &["b", "d"]),
        })
        .unwrap();
    driver.publish_provider_status(ProviderStatus {
        scene_version: 2,
        last_tick: handle.applied_tick(),
        width: 64,
        height: 64,
        fps_num: 25,
        fps_den: 1,
    });
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            request("select_preview", Some("e")),
            "rejected-unaware",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    advance(&mut driver, 11, 8);
    assert_eq!(
        handle.snapshot().unwrap().preview.as_str(),
        "d",
        "a rejected selection never airs (the effective preview is unchanged)"
    );
    let report = leases.sweep(
        Some(handle.snapshot().unwrap().preview.as_str()),
        handle.composed_count(),
        Instant::now() + Duration::from_secs(5),
        64,
    );
    assert_eq!(
        report.dropped, 1,
        "only the never-applied replacement is released"
    );
    assert_eq!(report.adopted, 0);
    assert!(
        leases.has_current(),
        "the live lease survives a rejected selection"
    );

    // ...and the rejected selection can never air later: a later pass that would
    // have adopted `e` finds nothing to adopt.
    let report = leases.sweep(
        Some("e"),
        handle.composed_count(),
        Instant::now() + Duration::from_secs(10),
        64,
    );
    assert_eq!(report, SwitcherLeaseSweep::default());
    assert_eq!(leases.pending_count(), 0);
    assert!(leases.has_current());
}

/// A scoped generation cancel releases every pending selection and the live
/// lease, and removes the entries, so no later sweep can adopt a cancelled
/// operation.
#[tokio::test]
async fn detach_cancels_pending_so_a_later_sweep_cannot_adopt_them() {
    let (handle, _driver) = runtime();
    let log = Arc::new(LeaseLog::default());
    let leases = Arc::new(SwitcherPreviewLeases::default());
    leases.seed(SwitcherPreviewLease::new(Box::new(TaggedLease::new(
        "boot",
        Arc::clone(&log),
    ))));
    let h = harness_with({
        let log = Arc::clone(&log);
        let leases = Arc::clone(&leases);
        move |state| {
            let demand: SharedSourceDemand = Arc::new(LeaseDemand { log });
            let mut state = state
                .with_sources_store(configured_sources(&["a", "b", "c", "d", "e"]))
                .with_switcher(handle)
                .with_source_demand(demand);
            state.switcher_preview_lease = leases;
            state
        }
    });
    for key in ["detach-1", "detach-2"] {
        let response = send(
            &h.router,
            post_with_key(
                "/api/v1/switcher/commands/select_preview",
                OPERATOR_TOKEN,
                request("select_preview", Some("b")),
                key,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
    assert_eq!(leases.pending_count(), 2);
    let generation = leases.generation();

    assert_eq!(leases.detach(), 2, "both pending selections are cancelled");
    assert_eq!(leases.generation(), generation + 1);
    assert_eq!(leases.pending_count(), 0);
    assert!(!leases.has_current(), "detach releases the live lease too");
    assert_eq!(
        log.dropped().len(),
        3,
        "boot + both pending leases released"
    );

    // A later sweep — even one whose picture would have adopted them — finds
    // nothing: a cancelled operation can never resurrect a demand.
    assert_eq!(
        leases.sweep(
            Some("b"),
            u64::MAX,
            Instant::now() + Duration::from_secs(5),
            64
        ),
        SwitcherLeaseSweep::default()
    );
    assert!(!leases.has_current());

    // Late requests to the detached owner fail; only a new boot may admit.
    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            request("select_preview", Some("b")),
            "detach-3",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(leases.pending_count(), 0);
    assert!(!leases.has_current());
}

/// The production loop itself (never a test-only path) reconciles a lost
/// outcome within its bounded cadence, off the output clock.
#[tokio::test]
async fn the_offclock_sweep_loop_reconciles_without_any_relay() {
    let (handle, mut driver) = runtime();
    let log = Arc::new(LeaseLog::default());
    let leases = Arc::new(SwitcherPreviewLeases::default());
    leases.seed(SwitcherPreviewLease::new(Box::new(TaggedLease::new(
        "boot",
        Arc::clone(&log),
    ))));
    let engine = handle.clone();
    let h = harness_with({
        let log = Arc::clone(&log);
        let leases = Arc::clone(&leases);
        move |state| {
            let demand: SharedSourceDemand = Arc::new(LeaseDemand { log });
            let mut state = state
                .with_sources_store(configured_sources(&["a", "b", "c", "d", "e"]))
                .with_switcher(engine.clone())
                .with_source_demand(demand);
            state.switcher_preview_lease = leases;
            state
        }
    });
    let stop = StopSignal::new();
    let sweeper = tokio::spawn(multiview_control::switcher_events::sweep_switcher_leases(
        handle.clone(),
        Arc::clone(&leases),
        stop.clone(),
        Duration::from_millis(20),
    ));

    let response = send(
        &h.router,
        post_with_key(
            "/api/v1/switcher/commands/select_preview",
            OPERATOR_TOKEN,
            request("select_preview", Some("d")),
            "loop-reconcile",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(leases.pending_count(), 1);
    // The engine applies it; the relay is dead, so only the sweep can resolve it.
    advance(&mut driver, 0, 12);

    let end = Instant::now() + Duration::from_secs(3);
    while leases.pending_count() > 0 && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        leases.pending_count(),
        0,
        "the off-clock sweep reconciled the lost outcome; a preview demand must not leak forever"
    );
    assert!(
        leases.has_current(),
        "the applied selection became the live lease"
    );
    assert!(log.dropped().iter().any(|tag| tag == "boot"));
    stop.stop();
    let _ = sweeper.await;
}

/// The composed-tick drain bound really covers a full pending storm: the mailbox
/// holds at most `MAX_PENDING` accepted selections and the engine applies a
/// fixed batch per tick, so a selection is decided within the bound — which is
/// what makes the sweep's drop decision terminal instead of a wall-clock guess.
#[test]
fn drain_bound_covers_a_full_pending_storm() {
    let batch = multiview_engine::switcher_control::MAX_COMMANDS_PER_TICK;
    let worst_ticks = u64::try_from(SwitcherPreviewLeases::MAX_PENDING.div_ceil(batch)).unwrap();
    assert!(
        SwitcherPreviewLeases::DRAIN_TICKS_BOUND > worst_ticks,
        "the drain bound must exceed a full storm's worst-case drain latency"
    );
}

/// Skipped output indices cannot masquerade as six command-drain passes.
#[test]
fn skipped_ticks_do_not_expire_outstanding_reservations() {
    let (handle, mut driver) = runtime();
    advance(&mut driver, 1_000_000, 1);
    assert_eq!(handle.composed_count(), 1);
    let log = Arc::new(LeaseLog::default());
    let leases = SwitcherPreviewLeases::default();
    assert!(leases
        .admit(
            "op".into(),
            "a".into(),
            SwitcherPreviewLease::new(Box::new(TaggedLease::new("a", log))),
            &handle,
            || Ok(handle.composed_count())
        )
        .is_ok());
    advance(&mut driver, 2_000_000, 1);
    assert_eq!(
        leases.sweep(
            Some("b"),
            handle.composed_count(),
            Instant::now() + Duration::from_secs(5),
            64
        ),
        SwitcherLeaseSweep::default()
    );
    assert_eq!(leases.pending_count(), 1);
    assert_eq!(leases.detach(), 1);
}

#[test]
fn repeated_boots_release_every_lease_and_reject_late_admission() {
    let log = Arc::new(LeaseLog::default());
    for boot in 0..32 {
        let (handle, _driver) = runtime();
        let leases = SwitcherPreviewLeases::default();
        leases.seed(SwitcherPreviewLease::new(Box::new(TaggedLease::new(
            "boot",
            Arc::clone(&log),
        ))));
        for n in 0..64 {
            assert!(leases
                .admit(
                    format!("{boot}-{n}"),
                    "a".into(),
                    SwitcherPreviewLease::new(Box::new(TaggedLease::new(
                        "pending",
                        Arc::clone(&log)
                    ))),
                    &handle,
                    || Ok(0)
                )
                .is_ok());
        }
        assert_eq!(leases.detach(), 64);
        leases.resolve(&format!("{boot}-0"), true);
        assert!(!leases.has_current());
        assert!(leases
            .admit(
                "late".into(),
                "a".into(),
                SwitcherPreviewLease::new(Box::new(TaggedLease::new("late", Arc::clone(&log)))),
                &handle,
                || panic!("closed owner enqueued")
            )
            .is_err());
        assert_eq!(log.dropped().len(), (boot + 1) * 66);
    }
}

#[tokio::test]
async fn cold_take_failure_never_swaps_program() {
    let (handle, mut driver) = runtime();
    let engine = handle.clone();
    let h = harness_with(move |state| {
        state
            .with_sources_store(configured_sources(&["a", "b", "c", "d", "e", "cold"]))
            .with_switcher(engine)
    });
    for (action, source, key) in [
        ("select_preview", Some("cold"), "cold-pvw"),
        ("cut", None, "cold-cut"),
    ] {
        let response = send(
            &h.router,
            post_with_key(
                &format!("/api/v1/switcher/commands/{action}"),
                OPERATOR_TOKEN,
                request(action, source),
                key,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        advance(&mut driver, if action == "cut" { 2 } else { 1 }, 1);
    }
    advance(&mut driver, 3, 60);
    assert_eq!(handle.snapshot().unwrap().program.as_str(), "a");
    assert_eq!(handle.snapshot().unwrap().preview.as_str(), "cold");
}

#[test]
fn reversed_success_receipts_preserve_the_latest_preview_demand() {
    use multiview_engine::switcher_control::{SwitcherCommand, SwitcherVerb};
    let (handle, mut driver) = runtime();
    let log = Arc::new(LeaseLog::default());
    let leases = SwitcherPreviewLeases::default();
    for (id, source) in [("first", "a"), ("second", "d")] {
        assert!(leases
            .admit(
                id.into(),
                source.into(),
                SwitcherPreviewLease::new(Box::new(TaggedLease::new(source, Arc::clone(&log)))),
                &handle,
                || {
                    handle
                        .try_send(SwitcherCommand {
                            operation_id: id.into(),
                            scene_version: 1,
                            verb: SwitcherVerb::SelectPreview(SourceRef::new(source)),
                        })
                        .map_err(|_| ())?;
                    Ok(handle.composed_count())
                }
            )
            .is_ok());
    }
    advance(&mut driver, 1, 1);
    assert_eq!(handle.snapshot().unwrap().preview.as_str(), "d");
    leases.resolve("second", true);
    leases.resolve("first", true);
    assert_eq!(leases.pending_count(), 0);
    assert!(leases.has_current());
    assert_eq!(log.dropped(), vec!["a"]);
    leases.detach();
    assert_eq!(log.dropped(), vec!["a", "d"]);
}
