#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::sync::Arc;
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
#[tokio::test]
async fn owned_hook_cut_changes_actual_composited_program_without_false_202() {
    let color = ColorInfo::default().resolve_defaults(64, 64);
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    a.publish(
        Nv12Image::solid(64, 64, 200, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    b.publish(
        Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    let stores = HashMap::from([("a".into(), a), ("b".into(), b)]);
    let drive = CompositorDrive::new(
        layout("a"),
        stores.clone(),
        Nv12Image::solid(64, 64, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("pgm".into())]);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: layout("a"),
            preview: layout("b"),
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
    let mut results = Vec::new();
    let mut outcomes = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "one".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
            expect: None,
        })
        .unwrap();
    let pace = Arc::clone(&clock);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            4,
            |frame| {
                results.push(frame.canvas.sample(8, 8).unwrap().0);
                pace.advance(std::time::Duration::from_millis(40));
                frame.canvas.sample(8, 8).unwrap().0
            },
            |_| None,
            SwitcherFrameHook::new(
                driver,
                "pgm".into(),
                |_: &mut CompositorDrive<Nv12Image>| {},
            ),
        )
        .await
        .unwrap();
    assert!(
        results.iter().all(|&y| y < 100),
        "first and all subsequent PGM pictures must show selected b: {results:?}"
    );
    let outcome = outcomes.try_recv().unwrap();
    assert_eq!(outcome.event.tick, 0);
    assert!(outcome.event.completed);
    assert!(outcomes.try_recv().is_err());
}

#[test]
fn failed_rebind_rolls_back_selection_and_never_airs_later() {
    use multiview_engine::runtime::FrameControl;
    let color = ColorInfo::default().resolve_defaults(64, 64);
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    a.publish(
        Nv12Image::solid(64, 64, 200, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    b.publish(
        Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    let stores = HashMap::from([("a".into(), Arc::clone(&a)), ("b".into(), Arc::clone(&b))]);
    let drive = CompositorDrive::new(
        layout("a"),
        stores.clone(),
        Nv12Image::solid(64, 64, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap();
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: layout("a"),
            preview: layout("b"),
        })
        .unwrap();
    let mut hook = SwitcherFrameHook::new(
        driver,
        "missing-cell-id".into(),
        |_: &mut CompositorDrive<Nv12Image>| {},
    );
    let mut drive = drive;
    let mut outcomes = handle.outcomes();
    handle
        .try_send(SwitcherCommand {
            operation_id: "failed-cut".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
            expect: None,
        })
        .unwrap();
    hook.before_compose(
        &mut drive,
        multiview_engine::clock::Tick {
            index: 0,
            pts: multiview_core::time::MediaTime::ZERO,
        },
    );
    assert!(outcomes.try_recv().unwrap().event.error.is_some());
    assert!(
        handle.snapshot().is_none(),
        "a failed initial frame cannot publish a guessed bus"
    );
    hook.after_compose(
        &drive
            .compose(multiview_engine::clock::Tick {
                index: 0,
                pts: multiview_core::time::MediaTime::ZERO,
            })
            .unwrap(),
    );
    for index in 1..5 {
        let tick = multiview_engine::clock::Tick {
            index,
            pts: multiview_core::time::MediaTime::ZERO,
        };
        hook.before_compose(&mut drive, tick);
        hook.after_compose(&drive.compose(tick).unwrap());
        assert!(
            handle.snapshot().is_none(),
            "invalid provider cannot pretend PGM state"
        );
        assert!(
            outcomes.try_recv().is_err(),
            "failed Cut cannot later air without new accepted op"
        );
    }
}

/// #341: a re-patch of the input on PGM (`on_air = allow`) rebinds the Program
/// cell to the new source on the patch tick itself, with no machine take.
#[test]
fn repatching_the_program_input_rebinds_the_program_cell_on_the_same_tick() {
    use multiview_engine::runtime::FrameControl;
    use multiview_engine::switcher_control::PatchSet;
    use multiview_engine::switcher_inputs::MixerInputs;
    let color = ColorInfo::default().resolve_defaults(64, 64);
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    a.publish(
        Nv12Image::solid(64, 64, 200, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    b.publish(
        Nv12Image::solid(64, 64, 60, 128, 128, color).unwrap(),
        multiview_core::time::MediaTime::ZERO,
    );
    let stores = HashMap::from([("a".into(), a), ("b".into(), b)]);
    let mut drive = CompositorDrive::new(
        layout("a"),
        stores.clone(),
        Nv12Image::solid(64, 64, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("pgm".into())]);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::input(1),
        SourceRef::input(2),
        vec![],
    ));
    driver.set_prime_stores(stores);
    driver.set_inputs(
        MixerInputs::new([
            (1, None, Some(SourceRef::new("a"))),
            (2, None, Some(SourceRef::new("b"))),
        ])
        .unwrap(),
    );
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: layout("a"),
            preview: layout("b"),
        })
        .unwrap();
    let mut hook = SwitcherFrameHook::new(
        driver,
        "pgm".into(),
        |_: &mut CompositorDrive<Nv12Image>| {},
    );
    let mut outcomes = handle.outcomes();
    let mut luma = Vec::new();
    for index in 0..3 {
        if index == 1 {
            handle
                .try_send(SwitcherCommand {
                    operation_id: "repatch".into(),
                    scene_version: 1,
                    verb: SwitcherVerb::Patch(PatchSet {
                        members: vec![(1, Some(SourceRef::new("b")))],
                        allow_on_air: true,
                    }),
                    expect: None,
                })
                .unwrap();
        }
        let tick = multiview_engine::clock::Tick {
            index,
            pts: multiview_core::time::MediaTime::ZERO,
        };
        hook.before_compose(&mut drive, tick);
        let frame = drive.compose(tick).unwrap();
        luma.push(frame.canvas.sample(8, 8).unwrap().0);
        hook.after_compose(&frame);
    }
    assert!(luma[0] > 150, "input 1 = a before the patch: {luma:?}");
    assert!(
        luma[1] < 100 && luma[2] < 100,
        "b from the patch tick on: {luma:?}"
    );
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!((outcome.tick, outcome.completed), (1, true));
    let snapshot = handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert_eq!(snapshot.program_input, Some(1));
    assert_eq!(snapshot.completion_count, 0, "a patch is not a faked take");
}

type Hook = SwitcherFrameHook<fn(&mut CompositorDrive<Nv12Image>)>;

/// A two-source M/E (PGM `a`, PVW `b`) whose hook publishes bus sizing.
struct SizingRig {
    drive: CompositorDrive<Nv12Image>,
    handle: SwitcherControl,
    hook: Hook,
    program: multiview_engine::switcher_hook::BusSizingSlot,
    preview: multiview_engine::switcher_hook::BusSizingSlot,
    /// The two source stores, to keep them live across a long run.
    sources: [Arc<TileStore<Nv12Image>>; 2],
}

/// The rig's no-op legacy control hook.
fn legacy(_: &mut CompositorDrive<Nv12Image>) {}

fn sizing_rig() -> SizingRig {
    use multiview_framestore::SwapCell;
    let color = ColorInfo::default().resolve_defaults(64, 64);
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    for store in [&a, &b] {
        store.publish(
            Nv12Image::solid(64, 64, 100, 128, 128, color).unwrap(),
            multiview_core::time::MediaTime::ZERO,
        );
    }
    let sources = [Arc::clone(&a), Arc::clone(&b)];
    let stores = HashMap::from([("a".into(), a), ("b".into(), b)]);
    let drive = CompositorDrive::new(
        layout("a"),
        stores.clone(),
        Nv12Image::solid(64, 64, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("pgm".into())]);
    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(stores);
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: layout("a"),
            preview: layout("b"),
        })
        .unwrap();
    let mut hook: Hook = SwitcherFrameHook::new(driver, "pgm".into(), legacy);
    let program = Arc::new(SwapCell::new(None));
    let preview = Arc::new(SwapCell::new(None));
    hook.bus_sizing = Some([Arc::clone(&program), Arc::clone(&preview)]);
    SizingRig {
        drive,
        handle,
        hook,
        program,
        preview,
        sources,
    }
}

/// #469 (b): the hook publishes the PGM/PVW sources for decode sizing on the
/// tick a CUT applies, before the frame is composed, and stores nothing on a
/// tick where the selections did not change (no per-tick allocation).
#[test]
fn the_hook_publishes_bus_sizing_only_when_a_selection_changes() {
    use multiview_engine::runtime::FrameControl;
    use multiview_framestore::SwapCell;
    let SizingRig {
        mut drive,
        handle,
        mut hook,
        program,
        preview,
        ..
    } = sizing_rig();
    let read = |slot: &SwapCell<Option<Arc<str>>>| slot.load().map(|s| s.to_string());
    let tick = |index| multiview_engine::clock::Tick {
        index,
        pts: multiview_core::time::MediaTime::ZERO,
    };
    hook.before_compose(&mut drive, tick(0));
    assert_eq!(read(&program).as_deref(), Some("a"));
    assert_eq!(read(&preview).as_deref(), Some("b"));
    let frame = drive.compose(tick(0)).unwrap();
    hook.after_compose(&frame);
    let held = program.load().unwrap();
    hook.before_compose(&mut drive, tick(1));
    assert!(
        Arc::ptr_eq(&held, &program.load().unwrap()),
        "an unchanged selection is not republished"
    );
    let frame = drive.compose(tick(1)).unwrap();
    hook.after_compose(&frame);
    handle
        .try_send(SwitcherCommand {
            operation_id: "cut".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
            expect: None,
        })
        .unwrap();
    hook.before_compose(&mut drive, tick(2));
    assert_eq!(
        read(&program).as_deref(),
        Some("b"),
        "the new PGM source is published before its first frame composes"
    );
    assert_eq!(read(&preview).as_deref(), Some("a"));
}

/// A CUT with no selection guard (#307), scene version 1.
fn unguarded_cut(operation_id: String) -> SwitcherCommand {
    SwitcherCommand {
        operation_id,
        scene_version: 1,
        verb: SwitcherVerb::Cut,
        expect: None,
    }
}

/// #469 chaos (inv #1/#10): a CUT on every tick while a decode-side reader
/// hammers the bus sizing slots. Sources stay live so the CUTs really apply
/// and the hook really republishes. The clock emits exactly one frame per
/// tick, the reader never blocks it, and the slots end on the final
/// selection.
#[tokio::test]
async fn a_cut_flood_with_a_hammering_sizing_reader_keeps_every_tick() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    const TICKS: u64 = 300;
    const TICK_NS: u64 = 40_000_000;
    let SizingRig {
        drive,
        handle,
        hook,
        program,
        preview,
        sources,
    } = sizing_rig();
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let reader = {
        let (program, preview, stop, reads) = (
            Arc::clone(&program),
            Arc::clone(&preview),
            Arc::clone(&stop),
            Arc::clone(&reads),
        );
        // Contention only: this thread asserts nothing it observes.
        std::thread::spawn(move || {
            let mut announced = false;
            while !stop.load(Ordering::Acquire) {
                let _p = program.load();
                let _v = preview.load();
                reads.fetch_add(1, Ordering::Relaxed);
                if !announced {
                    ready_tx.send(()).unwrap();
                    announced = true;
                }
            }
        })
    };
    ready_rx.recv().unwrap();
    let color = ColorInfo::default().resolve_defaults(64, 64);
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
    let pace = Arc::clone(&clock);
    let flood = handle.clone();
    // What the slot held after each fresh tick, read on the clock thread
    // itself (the frame callback), so the observation is deterministic.
    let mut observed: Vec<Option<Arc<str>>> = Vec::new();
    let watch = Arc::clone(&program);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                frames.push(frame.tick.index);
                observed.push(watch.load());
                // One CUT per tick from the control side; a full mailbox
                // drops, it never waits.
                let _ = flood.try_send(unguarded_cut(format!("cut{}", frame.tick.index)));
                pace.advance(std::time::Duration::from_nanos(TICK_NS));
                let next = i64::try_from((frame.tick.index + 1) * TICK_NS).unwrap();
                for store in &sources {
                    store.publish(
                        Nv12Image::solid(64, 64, 100, 128, 128, color).unwrap(),
                        multiview_core::time::MediaTime::from_nanos(next),
                    );
                }
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    stop.store(true, Ordering::Release);
    reader.join().unwrap();
    let changes = observed.windows(2).filter(|w| w[0] != w[1]).count();
    assert_eq!(
        frames,
        (0..TICKS).collect::<Vec<_>>(),
        "exactly one frame per tick"
    );
    let snapshot = handle.snapshot().unwrap();
    let applied = snapshot.completion_count;
    assert!(applied > TICKS / 2, "the CUTs really applied: {applied}");
    assert!(reads.load(Ordering::Relaxed) > 0, "the reader ran");
    assert!(
        changes > usize::try_from(TICKS / 2).unwrap(),
        "the hook republished the alternating PGM source: {changes}"
    );
    assert_eq!(
        program.load().as_deref(),
        Some(snapshot.program.as_str()),
        "PGM sizing ends on the final selection"
    );
    assert_eq!(preview.load().as_deref(), Some(snapshot.preview.as_str()));
}

/// #469 review I1: a CUT whose compose then fails (a GPU readback deadline)
/// is rolled back; the decode sizing follows the rollback at once instead of
/// keeping the rejected selection.
#[test]
fn a_failed_compose_republishes_the_rolled_back_bus_sizing() {
    use multiview_engine::runtime::FrameControl;
    let SizingRig {
        mut drive,
        handle,
        mut hook,
        program,
        preview,
        ..
    } = sizing_rig();
    let tick = |index| multiview_engine::clock::Tick {
        index,
        pts: multiview_core::time::MediaTime::ZERO,
    };
    hook.before_compose(&mut drive, tick(0));
    let frame = drive.compose(tick(0)).unwrap();
    hook.after_compose(&frame);
    handle
        .try_send(SwitcherCommand {
            operation_id: "cut".into(),
            scene_version: 1,
            verb: SwitcherVerb::Cut,
            expect: None,
        })
        .unwrap();
    hook.before_compose(&mut drive, tick(1));
    assert_eq!(program.load().as_deref(), Some("b"), "the cut is published");
    hook.on_compose_failed(&mut drive, tick(1), "readback deadline");
    let snapshot_program = hook.driver.resolved_selections().0.as_str().to_owned();
    assert_eq!(snapshot_program, "a", "the machine rolled back");
    assert_eq!(
        program.load().as_deref(),
        Some("a"),
        "sizing follows the rollback"
    );
    assert_eq!(preview.load().as_deref(), Some("b"));
}
