//! ADR-0104 (issue #251): the output clock loses zero ticks when the selected
//! source never produces a frame, during a CUT and during an AUTO; a take never
//! puts a stale or slate picture on Program; AUTO dissolves over exactly N ticks.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherOutcome, SwitcherVerb,
    TAKE_PRIME_DEADLINE_TICKS,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::sync::Arc;

const TICK_NS: i64 = 40_000_000;
const A_Y: u8 = 200;
const B_Y: u8 = 60;

fn layout(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: "program".into(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
            source: Some(id.into()),
            ..Cell::default()
        }],
    })
}

fn solid(y: u8) -> Nv12Image {
    Nv12Image::solid(
        64,
        64,
        y,
        128,
        128,
        ColorInfo::default().resolve_defaults(64, 64),
    )
    .unwrap()
}

/// Keep a source live: one frame stamped at every output tick of the run.
fn feed(store: &TileStore<Nv12Image>, y: u8, ticks: u64) {
    for i in 0..ticks {
        store.publish(
            solid(y),
            MediaTime::from_nanos(i64::try_from(i).unwrap() * TICK_NS),
        );
    }
}

struct Run {
    /// (tick index, PGM luma at a probe pixel) for every emitted frame.
    frames: Vec<(u64, u8)>,
    outcomes: Vec<SwitcherOutcome>,
    handle: SwitcherControl,
}

/// Drive `ticks` real output ticks through `EngineRuntime` + the switcher
/// hook. `program` = boot PGM, `preview` = boot PVW; `commands` are queued at
/// the tick index paired with each.
async fn run(
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    program: &str,
    preview: &str,
    ticks: u64,
    commands: Vec<(u64, SwitcherVerb)>,
) -> Run {
    let drive = CompositorDrive::new(
        layout(program),
        stores.clone(),
        solid(16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("cell-pgm".into())]);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new(program),
        SourceRef::new(preview),
        vec![],
    ));
    driver.set_prime_stores(stores);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: layout(program),
            preview: layout(preview),
        })
        .unwrap();
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut frames = Vec::new();
    let mut events = handle.outcomes();
    let sender = handle.clone();
    let mut pending = commands.into_iter().enumerate().collect::<Vec<_>>();
    let pace = Arc::clone(&clock);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            ticks,
            |frame| {
                frames.push((frame.tick.index, frame.canvas.sample(8, 8).unwrap().0));
                // Queue the commands due at the NEXT tick (control plane is
                // off-clock: a try_send, never a wait).
                let next = frame.tick.index + 1;
                pending.retain(|(op, (at, verb))| {
                    if *at == next {
                        sender
                            .try_send(SwitcherCommand {
                                operation_id: op.to_string(),
                                scene_version: 1,
                                verb: verb.clone(),
                            })
                            .unwrap();
                        false
                    } else {
                        true
                    }
                });
                pace.advance(std::time::Duration::from_millis(40));
                0
            },
            |_| None,
            // #245: the Program cell id is not the program id.
            SwitcherFrameHook::new(
                driver,
                "cell-pgm".into(),
                |_: &mut CompositorDrive<Nv12Image>| {},
            ),
        )
        .await
        .unwrap();
    let mut outcomes = Vec::new();
    while let Ok(item) = events.try_recv() {
        outcomes.push((*item.event).clone());
    }
    Run {
        frames,
        outcomes,
        handle,
    }
}

fn assert_every_tick(frames: &[(u64, u8)], ticks: u64) {
    let indices: Vec<u64> = frames.iter().map(|(i, _)| *i).collect();
    assert_eq!(
        indices,
        (0..ticks).collect::<Vec<_>>(),
        "exactly one frame per output tick, none lost or repeated"
    );
}

#[tokio::test]
async fn a_take_to_a_source_that_never_produces_loses_zero_ticks_and_is_refused() {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    let dead = Arc::new(TileStore::<Nv12Image>::with_defaults("dead"));
    let ticks = 2 * TAKE_PRIME_DEADLINE_TICKS + 30;
    feed(&a, A_Y, ticks);
    feed(&b, B_Y, ticks);
    let stores = HashMap::from([("a".into(), a), ("b".into(), b), ("dead".into(), dead)]);
    let run = run(
        stores,
        "a",
        "b",
        ticks,
        vec![
            (2, SwitcherVerb::SelectPreview(SourceRef::new("dead"))),
            (4, SwitcherVerb::Cut),
            (
                TAKE_PRIME_DEADLINE_TICKS + 10,
                SwitcherVerb::SelectProgram(SourceRef::new("dead")),
            ),
        ],
    )
    .await;
    assert_every_tick(&run.frames, ticks);
    assert!(
        run.frames.iter().all(|(_, y)| *y == A_Y),
        "Program never shows the slate of a source without a fresh frame"
    );
    let cut = run.outcomes.iter().find(|o| o.operation_id == "1").unwrap();
    assert!(!cut.completed);
    assert_eq!(cut.tick, 4 + TAKE_PRIME_DEADLINE_TICKS, "bounded hold");
    assert!(cut.error.as_deref().unwrap().contains("fresh frame"));
    let punch = run.outcomes.iter().find(|o| o.operation_id == "2").unwrap();
    assert!(!punch.completed, "a PGM punch has the same gate");
    assert_eq!(punch.tick, 2 * TAKE_PRIME_DEADLINE_TICKS + 10);
    let snapshot = run.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "a");
}

#[tokio::test]
async fn a_take_to_a_stale_source_waits_and_is_refused_instead_of_airing_a_held_frame() {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let stale = Arc::new(TileStore::<Nv12Image>::with_defaults("stale"));
    let ticks = TAKE_PRIME_DEADLINE_TICKS + 40;
    feed(&a, A_Y, ticks);
    // Primed once at t=0, then dead: `is_primed` is true, but not fresh.
    stale.publish(solid(B_Y), MediaTime::ZERO);
    let stores = HashMap::from([("a".into(), a), ("stale".into(), stale)]);
    let run = run(stores, "a", "stale", ticks, vec![(30, SwitcherVerb::Cut)]).await;
    assert_every_tick(&run.frames, ticks);
    assert!(run.frames.iter().all(|(_, y)| *y == A_Y));
    let cut = &run.outcomes[0];
    assert!(!cut.completed);
    assert_eq!(cut.tick, 30 + TAKE_PRIME_DEADLINE_TICKS);
}

#[tokio::test]
async fn cut_to_a_third_source_loses_zero_ticks_and_switches_on_its_tick() {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    let c = Arc::new(TileStore::<Nv12Image>::with_defaults("c"));
    let ticks = 40;
    feed(&a, A_Y, ticks);
    feed(&b, B_Y, ticks);
    feed(&c, 120, ticks);
    let stores = HashMap::from([("a".into(), a), ("b".into(), b), ("c".into(), c)]);
    let run = run(
        stores,
        "a",
        "b",
        ticks,
        vec![
            (5, SwitcherVerb::SelectPreview(SourceRef::new("c"))),
            (10, SwitcherVerb::Cut),
            (20, SwitcherVerb::Cut),
        ],
    )
    .await;
    assert_every_tick(&run.frames, ticks);
    for (index, y) in &run.frames {
        let want = if *index < 10 {
            A_Y
        } else if *index < 20 {
            120
        } else {
            A_Y
        };
        assert_eq!(*y, want, "tick {index}");
    }
    assert!(run
        .outcomes
        .iter()
        .all(|o| o.completed && o.error.is_none()));
    let snapshot = run.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "a");
    assert_eq!(snapshot.preview.as_str(), "c");
}

#[tokio::test]
async fn auto_dissolves_over_exactly_n_ticks_and_loses_zero_ticks() {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    let ticks = 40;
    feed(&a, A_Y, ticks);
    feed(&b, B_Y, ticks);
    let stores = HashMap::from([("a".into(), a), ("b".into(), b)]);
    let n = 10_u32;
    let start = 5_u64;
    let run = run(
        stores,
        "a",
        "b",
        ticks,
        vec![
            (start, SwitcherVerb::Auto(FrameCount::try_from(n).unwrap())),
            // A second AUTO during the flight is refused.
            (
                start + 3,
                SwitcherVerb::Auto(FrameCount::try_from(n).unwrap()),
            ),
        ],
    )
    .await;
    assert_every_tick(&run.frames, ticks);
    assert!(run.handle.supports_auto());
    let end = start + u64::from(n);
    let mut previous = u8::MAX;
    for (index, y) in &run.frames {
        if *index <= start {
            assert_eq!(
                *y, A_Y,
                "tick {index}: before/at start only the outgoing picture"
            );
        } else if *index < end {
            assert!(
                *y < previous && *y > B_Y && *y < A_Y,
                "tick {index}: strictly dissolving ({y}, previous {previous})"
            );
        } else {
            assert_eq!(*y, B_Y, "tick {index}: take completed");
        }
        previous = *y;
    }
    let auto = run.outcomes.iter().find(|o| o.operation_id == "0").unwrap();
    assert!(auto.completed);
    assert_eq!(auto.tick, end, "completes exactly N ticks after it started");
    let second = run.outcomes.iter().find(|o| o.operation_id == "1").unwrap();
    assert!(!second.completed);
    assert!(second.error.as_deref().unwrap().contains("second AUTO"));
    let snapshot = run.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert_eq!(snapshot.preview.as_str(), "a");
    assert!(!snapshot.in_flight);
}

#[tokio::test]
async fn auto_to_a_source_that_never_produces_loses_zero_ticks_and_never_blends_a_slate() {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let dead = Arc::new(TileStore::<Nv12Image>::with_defaults("dead"));
    let ticks = TAKE_PRIME_DEADLINE_TICKS + 20;
    feed(&a, A_Y, ticks);
    let stores = HashMap::from([("a".into(), a), ("dead".into(), dead)]);
    let run = run(
        stores,
        "a",
        "dead",
        ticks,
        vec![(3, SwitcherVerb::Auto(FrameCount::try_from(10).unwrap()))],
    )
    .await;
    assert_every_tick(&run.frames, ticks);
    assert!(run.frames.iter().all(|(_, y)| *y == A_Y));
    let auto = &run.outcomes[0];
    assert!(!auto.completed);
    assert_eq!(auto.tick, 3 + TAKE_PRIME_DEADLINE_TICKS);
}
