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
struct JumpPacer(Arc<ManualTimeSource>);

impl Pacer for JumpPacer {
    fn wait_until(
        &self,
        deadline_nanos: i64,
        source: &dyn TimeSource,
        _stop: &StopSignal,
    ) -> impl std::future::Future<Output = ()> + Send {
        if source.now_nanos() < deadline_nanos {
            self.0.set(deadline_nanos);
        }
        std::future::ready(())
    }
}

async fn run_with_compose_cost(cost: Duration, ticks: u64) -> (EngineRuntime<JumpPacer>, u64) {
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        build_drive(16, 16),
        ts,
        JumpPacer(Arc::clone(&time_source)),
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
