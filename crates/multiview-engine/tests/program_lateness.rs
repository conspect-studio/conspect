//! Program tick lateness (ADR-0108 §10): the runtime records emit time minus
//! deadline at every publish, fresh or repeat, into the documented buckets.
//! Driven by the real [`EngineRuntime`] with a [`ManualTimeSource`] (no sleeps).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::collections::HashMap;

use std::sync::Arc;
use std::time::Duration;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::{
    CompositorDrive, EnginePublisher, EngineRuntime, OutputClock, Pacer, StopSignal, TimeSource,
};
use multiview_framestore::TileStore;

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

/// A one-cell drive over a single never-fed store (every tile rides `NoSignal` —
/// a valid frame is still produced every tick).
fn build_drive(w: u32, h: u32) -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "cadence".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 25,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

/// A pacer that wakes exactly at the deadline by moving the manual clock
/// there (never backwards): the only lateness is what the test injects.
/// `wake_late` models OS wake latency: the pacer returns that long after the
/// deadline.
struct JumpPacer(Arc<ManualTimeSource>, i64);

impl Pacer for JumpPacer {
    fn wait_until(
        &self,
        deadline_nanos: i64,
        source: &dyn TimeSource,
        _stop: &StopSignal,
    ) -> impl std::future::Future<Output = ()> + Send {
        if source.now_nanos() < deadline_nanos {
            self.0.set(deadline_nanos.saturating_add(self.1));
        }
        std::future::ready(())
    }
}

async fn run_with_compose_cost(cost: Duration, ticks: u64) -> (EngineRuntime<JumpPacer>, u64) {
    run_with_wake_and_compose(Duration::ZERO, cost, ticks).await
}

async fn run_with_wake_and_compose(
    wake: Duration,
    cost: Duration,
    ticks: u64,
) -> (EngineRuntime<JumpPacer>, u64) {
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        build_drive(16, 16),
        ts,
        JumpPacer(
            Arc::clone(&time_source),
            i64::try_from(wake.as_nanos()).unwrap(),
        ),
    );
    // The control hook runs before compose and publish: advancing the clock
    // there makes every fresh tick publish exactly `cost` after its deadline.
    let ts_ctl = Arc::clone(&time_source);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| ts_ctl.advance(cost);
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &StopSignal::new(),
            ticks,
            |f| f.tick.index,
            |_f| None::<u64>,
            control,
        )
        .await
        .unwrap();
    (runtime, outcome.ticks)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_on_time_run_lands_in_the_first_bucket() {
    let (runtime, ticks) = run_with_compose_cost(Duration::ZERO, 50).await;
    let s = runtime.lateness().snapshot();
    assert_eq!(s.count, ticks);
    assert_eq!(s.buckets[0], ticks, "{s:?}");
    assert_eq!(s.max_ns, 0);
    assert_eq!(runtime.frames_repeated(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_publish_lands_in_its_bucket_and_repeats_are_recorded_too() {
    // 5 ms late each tick: the (4, 8] ms bucket, index 4.
    let (runtime, ticks) = run_with_compose_cost(Duration::from_millis(5), 50).await;
    let s = runtime.lateness().snapshot();
    assert_eq!(s.count, ticks);
    assert_eq!(s.buckets[4], ticks, "{s:?}");
    assert_eq!(s.max_ns, 5_000_000);
    assert_eq!(s.quantile_upper_ns(990), Some(8_000_000));
    // 100 ms per compose at 40 ms ticks: repeats fire, and each one is recorded.
    let (runtime, ticks) = run_with_compose_cost(Duration::from_millis(100), 60).await;
    let s = runtime.lateness().snapshot();
    assert!(runtime.frames_repeated() > 0);
    assert_eq!(s.count, ticks, "fresh and repeated ticks are both recorded");
    assert!(
        s.buckets[8] > 0,
        "a 100 ms late publish is in the overflow bucket: {s:?}"
    );
}

/// #311: the emit lateness splits into wake lateness (the pacer returning
/// after the deadline) and the compose duration (control hook through
/// publish), each attributed to the phase that spent it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_lateness_splits_into_wake_lateness_and_compose_phases() {
    // Woken 0.3 ms late, 5 ms spent in the control hook: emit 5.3 ms late.
    let (runtime, ticks) =
        run_with_wake_and_compose(Duration::from_micros(300), Duration::from_millis(5), 50).await;
    let emit = runtime.lateness().snapshot();
    let p = runtime.phases().snapshot();
    assert_eq!(emit.count, ticks);
    assert_eq!(p.wake.count, ticks, "every fresh tick records its wake");
    assert_eq!(p.compose.count, ticks);
    assert_eq!(p.wake.max_ns, 300_000);
    assert_eq!(p.wake.fine_quantile_upper_ns(990), Some(500_000));
    assert_eq!(p.compose.max_ns, 5_000_000);
    assert_eq!(p.hook.max_ns, 5_000_000, "the hook spent it");
    assert_eq!(p.composite.max_ns, 0);
    assert_eq!(p.after.max_ns, 0);
    assert_eq!(p.project.max_ns, 0);
    assert_eq!(emit.max_ns, 5_300_000, "emit = wake + compose");
    assert_eq!(
        emit.sum_ns,
        p.wake.sum_ns + p.compose.sum_ns,
        "on a fresh-only run the emit lateness is exactly wake + compose"
    );
    assert_eq!(runtime.frames_repeated(), 0);
}

/// #311 reopen: the composited canvas is shared, never copied, on the clock
/// thread. Every consumer of a tick holds the one allocation the compositor
/// produced, and a cadence-hold repeat re-emits that same allocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_canvas_is_shared_by_every_consumer_and_by_its_repeats() {
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        build_drive(16, 16),
        ts,
        JumpPacer(Arc::clone(&time_source), 0),
    );
    // 100 ms per compose at 40 ms ticks: every fresh tick is followed by repeats.
    let ts_ctl = Arc::clone(&time_source);
    let control =
        move |_d: &mut CompositorDrive<Nv12Image>| ts_ctl.advance(Duration::from_millis(100));
    let seen = std::sync::Mutex::new(Vec::new());
    runtime
        .run_for_with_control(
            publisher.as_ref(),
            &StopSignal::new(),
            30,
            |f| {
                // What a projection keeps (the encode hand-off, the preview).
                let held = Arc::clone(&f.canvas);
                seen.lock().unwrap().push((f.repeat, held));
                f.tick.index
            },
            |_f| None::<u64>,
            control,
        )
        .await
        .unwrap();
    let seen = seen.into_inner().unwrap();
    assert!(seen.iter().any(|(repeat, _)| *repeat), "repeats happened");
    let mut fresh: Option<&Arc<Nv12Image>> = None;
    for (repeat, canvas) in &seen {
        if *repeat {
            let fresh = fresh.expect("a repeat follows a fresh tick");
            assert!(
                Arc::ptr_eq(fresh, canvas),
                "a repeat re-emits the same canvas"
            );
        } else {
            if let Some(previous) = fresh {
                assert!(
                    !Arc::ptr_eq(previous, canvas),
                    "a fresh tick is a new canvas"
                );
            }
            fresh = Some(canvas);
        }
    }
}

/// Review L1 (#357): with cadence-hold repeats between fresh ticks, the wake
/// lateness is charged to the tick that is composed fresh (the freshest due
/// tick), never to an older tick the repeat walk already emitted. So a fresh
/// tick's wake stays under one period however far behind the loop is, and
/// the backlog shows up as repeats and emit lateness, not as wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_lateness_is_charged_to_the_fresh_tick_after_repeats() {
    // 100 ms per compose at 40 ms ticks: every fresh tick follows repeats.
    let (runtime, ticks) = run_with_compose_cost(Duration::from_millis(100), 60).await;
    let p = runtime.phases().snapshot();
    let emit = runtime.lateness().snapshot();
    assert!(runtime.frames_repeated() > 0);
    assert_eq!(emit.count, ticks);
    assert_eq!(
        p.wake.count + runtime.frames_repeated(),
        ticks,
        "one wake per fresh tick"
    );
    assert!(
        p.wake.max_ns < 40_000_000,
        "a fresh tick is the freshest due tick: its wake is under one period: {p:?}"
    );
    assert_eq!(p.compose.max_ns, 100_000_000);
}
