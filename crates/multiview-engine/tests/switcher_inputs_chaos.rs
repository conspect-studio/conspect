//! Chaos for the numbered mixer-input patch (#341, ADR-0110 §12.10; invariants
//! #1 and #10). Assertions are on counts and bounds only, never on wall-clock
//! percentiles, so they hold on a loaded CI runner.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::type_complexity
)]
// reason: a chaos test asserts many counts over one run; splitting it hides
// the single-run relationship between the flood and the clock.
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::SourceRef;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    PatchSet, ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb, COMMAND_CAPACITY,
    MAX_COMMANDS_PER_TICK, REFUSE_NO_FRESH_FRAME, TAKE_PRIME_DEADLINE_TICKS,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::switcher_inputs::MixerInputs;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const LUMA_A: u8 = 200;
const LUMA_B: u8 = 60;
const LUMA_BG: u8 = 16;

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

/// `a` and `b` are fresh at every tick the test drives (published far ahead
/// of the manual clock is not possible, so they are re-published each tick);
/// `cold` exists but never receives a frame.
struct Rig {
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    color: ColorInfo,
}

impl Rig {
    fn new() -> Self {
        let color = ColorInfo::default().resolve_defaults(64, 64);
        let stores = ["a", "b", "cold"]
            .into_iter()
            .map(|id| {
                (
                    id.to_owned(),
                    Arc::new(TileStore::<Nv12Image>::with_defaults(id)),
                )
            })
            .collect();
        let rig = Self { stores, color };
        rig.publish(MediaTime::ZERO);
        rig
    }

    fn publish(&self, at: MediaTime) {
        for (id, luma) in [("a", LUMA_A), ("b", LUMA_B)] {
            self.stores[id].publish(
                Nv12Image::solid(64, 64, luma, 128, 128, self.color).unwrap(),
                at,
            );
        }
    }

    fn driver(
        &self,
    ) -> (
        SwitcherControl,
        SwitcherFrameHook<impl FnMut(&mut CompositorDrive<Nv12Image>)>,
        CompositorDrive<Nv12Image>,
    ) {
        let drive = CompositorDrive::new(
            layout("a"),
            self.stores.clone(),
            Nv12Image::solid(64, 64, LUMA_BG, 128, 128, self.color).unwrap(),
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
        driver.set_prime_stores(self.stores.clone());
        driver.set_inputs(
            MixerInputs::new([
                (1, None, Some(SourceRef::new("a"))),
                (2, None, Some(SourceRef::new("b"))),
                (3, None, Some(SourceRef::new("a"))),
                (4, None, None),
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
        let hook = SwitcherFrameHook::new(
            driver,
            "pgm".into(),
            |_: &mut CompositorDrive<Nv12Image>| {},
        );
        (handle, hook, drive)
    }
}

fn patch(
    op: String,
    members: Vec<(u16, Option<SourceRef>)>,
    allow_on_air: bool,
) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op,
        scene_version: 1,
        verb: SwitcherVerb::Patch(PatchSet {
            members,
            allow_on_air,
        }),
    }
}

/// A producer floods the mailbox with every kind of patch while the real
/// runtime emits 600 ticks. The clock emits exactly one frame per tick, never
/// repeats, never waits on the flood; at most 16 commands apply per tick; every
/// accepted command gets exactly one terminal outcome; the on-air picture is
/// always one of the patched sources (never background) and matches the final
/// snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_patch_flood_never_moves_the_output_clock() {
    const TICKS: u64 = 600;
    let rig = Rig::new();
    let (handle, hook, drive) = rig.driver();
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut outcomes = handle.outcomes();

    let done = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let rejected_full = Arc::new(AtomicU64::new(0));
    let producer = {
        let handle = handle.clone();
        let done = Arc::clone(&done);
        let accepted = Arc::clone(&accepted);
        let rejected_full = Arc::clone(&rejected_full);
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while !done.load(Ordering::Acquire) {
                let op = format!("op-{n}");
                let command = match n % 6 {
                    // On-air cut of the PGM input between two fresh sources.
                    0 => patch(
                        op.clone(),
                        vec![(1, Some(SourceRef::new(if n % 12 == 0 { "b" } else { "a" })))],
                        true,
                    ),
                    // Off-air patch of a spare input.
                    1 => patch(
                        op.clone(),
                        vec![
                            (3, Some(SourceRef::new("b"))),
                            (4, Some(SourceRef::new("a"))),
                        ],
                        false,
                    ),
                    // Invalid: undeclared input.
                    2 => patch(op.clone(), vec![(9, Some(SourceRef::new("a")))], false),
                    // Invalid: unknown source.
                    3 => patch(op.clone(), vec![(3, Some(SourceRef::new("nope")))], false),
                    // Protected: PGM input without allow.
                    4 => patch(op.clone(), vec![(1, Some(SourceRef::new("b")))], false),
                    // Held then refused: PGM input to a never-fresh source.
                    _ => patch(op.clone(), vec![(1, Some(SourceRef::new("cold")))], true),
                };
                if handle.try_send(command).is_ok() {
                    accepted.lock().unwrap().push(op);
                } else {
                    rejected_full.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                n += 1;
            }
        })
    };

    let mut lumas = Vec::new();
    let mut indices = Vec::new();
    // The outcome stream is a bounded drop-oldest ring (invariant #10), so the
    // reader drains it every tick, as the control-plane relay does.
    let mut seen = HashSet::new();
    let mut per_tick: HashMap<u64, usize> = HashMap::new();
    let mut lagged = 0_u64;
    let pace = Arc::clone(&clock);
    let tick_ns = 40_000_000_u64;
    let outcome = runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                lumas.push(frame.canvas.sample(8, 8).unwrap().0);
                indices.push(frame.tick.index);
                loop {
                    match outcomes.try_recv() {
                        Ok(item) => {
                            assert!(
                                seen.insert(item.event.operation_id.clone()),
                                "one outcome per op"
                            );
                            *per_tick.entry(item.event.tick).or_default() += 1;
                        }
                        Err(multiview_engine::isolation::TryRecvError::Lagged(n)) => lagged += n,
                        Err(_) => break,
                    }
                }
                pace.advance(std::time::Duration::from_nanos(tick_ns));
                rig.publish(MediaTime::from_nanos(
                    i64::try_from((frame.tick.index + 1) * tick_ns).unwrap(),
                ));
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    done.store(true, Ordering::Release);
    producer.join().unwrap();
    let _ = outcome;

    // Invariant #1: exactly one frame per tick, contiguous, no repeats.
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    // The on-air picture is always a patched fresh source, never background.
    assert!(
        lumas.iter().all(|&y| y == LUMA_A
            || y == LUMA_B
            || y.abs_diff(LUMA_A) < 8
            || y.abs_diff(LUMA_B) < 8),
        "Program always shows a or b: {:?}",
        lumas
            .iter()
            .filter(|&&y| y.abs_diff(LUMA_A) >= 8 && y.abs_diff(LUMA_B) >= 8)
            .take(5)
            .collect::<Vec<_>>()
    );
    // Invariant #10: the producer saw a full mailbox, never a block.
    assert!(
        rejected_full.load(Ordering::Relaxed) > 0,
        "the flood filled the bounded mailbox"
    );

    // Every accepted command has exactly one terminal outcome, at most 16
    // command applications per tick. Commands still queued at the end are
    // bounded by the mailbox (plus the one held slot).
    let accepted = accepted.lock().unwrap().clone();
    assert_eq!(lagged, 0, "a per-tick reader never lags the outcome ring");
    let unresolved = accepted.iter().filter(|op| !seen.contains(*op)).count();
    assert!(
        unresolved <= COMMAND_CAPACITY + 1,
        "at most the mailbox and the held slot stay unresolved: {unresolved}"
    );
    // A tick can carry 16 drained commands plus the held command's outcome.
    assert!(
        per_tick.values().all(|&n| n <= MAX_COMMANDS_PER_TICK + 1),
        "bounded work per tick: {:?}",
        per_tick.values().max()
    );
    assert!(
        u64::try_from(seen.len()).unwrap() >= TICKS,
        "the flood was really processed: {}",
        seen.len()
    );

    // The last frame shows the resolved PGM source of the final snapshot.
    let snapshot = handle.snapshot().unwrap();
    let expected = if snapshot.program.as_str() == "a" {
        LUMA_A
    } else {
        LUMA_B
    };
    assert!(lumas.last().unwrap().abs_diff(expected) < 8);
}

/// A PGM re-patch to a source that never becomes fresh is held for exactly
/// the prime deadline, then refused; every tick of the hold still emits a
/// frame showing the old source, and the table is unchanged.
#[tokio::test]
async fn a_held_cold_patch_never_stalls_the_clock() {
    let rig = Rig::new();
    let (handle, hook, drive) = rig.driver();
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        drive,
        ts,
        CooperativePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut outcomes = handle.outcomes();
    handle
        .try_send(patch(
            "cold".into(),
            vec![(1, Some(SourceRef::new("cold")))],
            true,
        ))
        .unwrap();
    let ticks = TAKE_PRIME_DEADLINE_TICKS + 10;
    let mut lumas = Vec::new();
    let pace = Arc::clone(&clock);
    let tick_ns = 40_000_000_u64;
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            ticks,
            |frame| {
                lumas.push(frame.canvas.sample(8, 8).unwrap().0);
                pace.advance(std::time::Duration::from_nanos(tick_ns));
                rig.publish(MediaTime::from_nanos(
                    i64::try_from((frame.tick.index + 1) * tick_ns).unwrap(),
                ));
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    assert_eq!(
        u64::try_from(lumas.len()).unwrap(),
        ticks,
        "one frame per tick through the hold"
    );
    assert_eq!(repeated.load(Ordering::Acquire), 0);
    assert!(
        lumas.iter().all(|y| y.abs_diff(LUMA_A) < 8),
        "Program keeps input 1 = a throughout: {lumas:?}"
    );
    let outcome = outcomes.try_recv().unwrap().event;
    assert_eq!(outcome.operation_id, "cold");
    assert!(!outcome.completed);
    assert_eq!(outcome.tick, TAKE_PRIME_DEADLINE_TICKS);
    assert_eq!(outcome.error.as_deref(), Some(REFUSE_NO_FRESH_FRAME));
    assert!(outcomes.try_recv().is_err(), "exactly one outcome");
    let table = handle.inputs().unwrap();
    assert_eq!(table.version, 0, "nothing was applied");
}
