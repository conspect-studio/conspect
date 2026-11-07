#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::{ManualTimeSource, Tick, TimeSource};
use multiview_engine::drive::{Placement, SceneStates, SwitcherTickState};
use multiview_engine::{CompositorDrive, EnginePublisher, EngineRuntime, OutputClock, StopSignal};
use multiview_framestore::TileStore;

fn drive() -> CompositorDrive<Nv12Image> {
    let color = ColorInfo::default().resolve_defaults(64, 48);
    let slate = Nv12Image::solid(16, 16, 100, 128, 128, color).unwrap();
    let layout = Layout {
        name: "resolver-test".into(),
        canvas: Canvas {
            width: 16,
            height: 16,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![Cell {
            w: 1.0,
            h: 1.0,
            source: Some("cam".into()),
            ..Cell::default()
        }],
    };
    let mut stores = HashMap::new();
    // A registered, unprimed store still selects the slate but reports a
    // real source state; the opacity path is tested independently of ingest.
    stores.insert(
        "cam".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam")),
    );
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        slate,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

fn alternate(
    scenes: &SceneStates<'_>,
    _state: &SwitcherTickState,
    tick: Tick,
    placements: &mut Vec<Placement>,
) {
    placements.push(Placement {
        image_index: 0,
        dst_x: 0,
        dst_y: 0,
        dst_w: scenes.layout.canvas.width,
        dst_h: scenes.layout.canvas.height,
        opacity: if tick.index % 2 == 0 { 1.0 } else { 0.0 },
    });
}

#[tokio::test]
async fn tick_aware_hook_and_resolver_replay_identically_without_pool_growth() {
    async fn replay() -> Vec<(u64, Vec<u8>)> {
        let drive = drive().with_render_plan_resolver(alternate);
        let time = Arc::new(ManualTimeSource::new());
        let ts: Arc<dyn TimeSource> = time.clone();
        let mut runtime = EngineRuntime::new(
            OutputClock::new(Rational::FPS_60).unwrap(),
            drive,
            ts,
            multiview_engine::CooperativePacer,
        );
        let publisher = EnginePublisher::<u64, ()>::new(1);
        let mut frames = Vec::new();
        let outcome = runtime
            .run_for_with_tick_control(
                &publisher,
                &StopSignal::new(),
                8,
                |frame| {
                    frames.push((frame.tick.index, frame.canvas.y_plane().to_vec()));
                    frame.tick.index
                },
                |_| None,
                move |_drive, tick| {
                    assert!(tick.index < 8);
                    time.advance(Duration::from_nanos(16_666_667));
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.ticks, 8);
        frames
    }
    let first = replay().await;
    let second = replay().await;
    assert_eq!(first, second);
    assert_ne!(first[0].1, first[1].1);

    // The injected resolver writes into the same reusable placement pool.
    let drive = drive().with_render_plan_resolver(alternate);
    let mut clock = OutputClock::new(Rational::FPS_60).unwrap();
    drive.compose(clock.tick()).unwrap();
    let warm = drive.scratch_reservations();
    assert!(warm > 0);
    for _ in 0..64 {
        drive.compose(clock.tick()).unwrap();
    }
    assert_eq!(drive.scratch_reservations(), warm);
}

#[tokio::test]
async fn wedged_downstream_consumer_does_not_stall_tick_aware_hook() {
    let time = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::FPS_60).unwrap(),
        drive(),
        ts,
        multiview_engine::CooperativePacer,
    );
    let publisher = EnginePublisher::<u64, ()>::new(1);
    // Keep a receiver alive without draining it: tick one fills the
    // one-event ring, and every subsequent tick must still publish promptly.
    let mut wedged_events = publisher.subscribe();
    publisher.publish_state(0);
    let consumer_state = publisher.state.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        let snapshot = consumer_state.latest().expect("initial state exists");
        ready_tx.send(()).unwrap();
        // Model MP-1's blocked OS-thread consumer while retaining a state snapshot.
        release_rx.recv().unwrap();
        assert_eq!(*snapshot, 0);
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // Run the tick loop on a separate OS thread: a synchronous blocked publish
    // cannot freeze the test thread and evade the wall-clock deadline.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let producer = std::thread::spawn(move || {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = executor.block_on(runtime.run_for_with_tick_control(
            &publisher,
            &StopSignal::new(),
            50,
            |frame| frame.tick.index,
            |_| Some(()),
            move |_, _| time.advance(Duration::from_nanos(16_666_667)),
        ));
        done_tx
            .send((outcome.unwrap().ticks, runtime.frames_repeated()))
            .unwrap();
    });
    let result = done_rx.recv_timeout(Duration::from_secs(3));
    release_tx.send(()).unwrap();
    consumer.join().unwrap();
    let (ticks, repeated) = result.expect("50 ticks must finish while consumers are wedged");
    producer.join().unwrap();
    assert_eq!(ticks, 50);
    assert_eq!(repeated, 0);
    assert_eq!(wedged_events.len(), 50);
    assert!(matches!(
        wedged_events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(49))
    ));
}
