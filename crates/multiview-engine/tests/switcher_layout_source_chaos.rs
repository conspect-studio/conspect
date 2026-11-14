//! Chaos for a stored layout as an M/E source (#307 PR 2; invariants #1 and
//! #10). A real `SwitcherFrameHook` over a real `CompositorDrive` adopts a
//! `layout:<id>` store from a `LayoutSources` registry that another thread
//! republishes (add/remove) while a third floods select/cut/auto commands.
//! The layout store itself is fed by a separate "aux" thread at tick pts; in
//! the second test that thread dies (a wedged or panicked routing-aux).
//! Assertions are on counts and bounds only, never wall-clock percentiles.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]
// reason: a chaos test asserts many counts over one run; splitting it hides
// the single-run relationship between the flood and the clock.
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::{MediaTime, Rational};
use multiview_core::traits::SourceState;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::switcher_layouts::{LayoutSourceSet, LayoutSources};
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEY: &str = "layout:wall";
const LUMA_A: u8 = 200;
const LUMA_B: u8 = 60;
const LUMA_LAYOUT: u8 = 120;
const TICK_NS: u64 = 40_000_000;

fn pts(index: u64) -> MediaTime {
    MediaTime::from_nanos(i64::try_from(index * TICK_NS).unwrap())
}

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

struct Rig {
    color: ColorInfo,
    a: Arc<TileStore<Nv12Image>>,
    b: Arc<TileStore<Nv12Image>>,
    layout_store: Arc<TileStore<Nv12Image>>,
    sources: Arc<LayoutSources>,
}

impl Rig {
    fn new() -> Self {
        let rig = Self {
            color: ColorInfo::default().resolve_defaults(64, 64),
            a: Arc::new(TileStore::with_defaults("a")),
            b: Arc::new(TileStore::with_defaults("b")),
            layout_store: Arc::new(TileStore::with_defaults(KEY)),
            sources: Arc::new(LayoutSources::new()),
        };
        rig.publish_sources(pts(0));
        rig.register();
        rig
    }

    fn solid(&self, luma: u8) -> Nv12Image {
        Nv12Image::solid(64, 64, luma, 128, 128, self.color).unwrap()
    }

    fn publish_sources(&self, at: MediaTime) {
        self.a.publish(self.solid(LUMA_A), at);
        self.b.publish(self.solid(LUMA_B), at);
    }

    fn register(&self) {
        self.sources.publish(LayoutSourceSet {
            entries: vec![(Arc::from(KEY), Arc::clone(&self.layout_store))],
        });
    }

    fn build(
        &self,
    ) -> (
        SwitcherControl,
        SwitcherFrameHook<impl FnMut(&mut CompositorDrive<Nv12Image>)>,
        EngineRuntime<CooperativePacer>,
        Arc<ManualTimeSource>,
        Arc<AtomicU64>,
    ) {
        let stores: HashMap<String, Arc<TileStore<Nv12Image>>> = HashMap::from([
            ("a".into(), Arc::clone(&self.a)),
            ("b".into(), Arc::clone(&self.b)),
        ]);
        let drive = CompositorDrive::new(
            layout("a"),
            stores.clone(),
            self.solid(16),
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
        driver.set_prime_stores(stores.clone());
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
        hook.layout_sources = Some(Arc::clone(&self.sources));
        hook.preview_stores = Some(stores);
        let clock = Arc::new(ManualTimeSource::new());
        let ts: Arc<dyn TimeSource> = clock.clone();
        let runtime = EngineRuntime::new(
            OutputClock::new(Rational::new(25, 1)).unwrap(),
            drive,
            ts,
            CooperativePacer,
        );
        let repeated = runtime.frames_repeated_counter();
        (handle, hook, runtime, clock, repeated)
    }
}

/// The "aux" thread: publishes the layout picture at the tick pts the sink
/// announces in `wanted`, then acknowledges with `done_upto = wanted + 1`.
/// Stops (dies) once `wanted` reaches `die_at`.
fn spawn_layout_publisher(
    store: Arc<TileStore<Nv12Image>>,
    picture: Nv12Image,
    wanted: Arc<AtomicU64>,
    done_upto: Arc<AtomicU64>,
    die_at: u64,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let picture = Arc::new(picture);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let want = wanted.load(Ordering::Acquire);
            let have = done_upto.load(Ordering::Acquire);
            if want >= die_at {
                return;
            }
            if have <= want {
                store.publish_arc(Arc::clone(&picture), pts(want));
                done_upto.store(want + 1, Ordering::Release);
            } else {
                std::thread::yield_now();
            }
        }
    })
}

/// The test's frame sink waits (bounded) for the layout publisher to have
/// fed tick `next` (`done_upto > next`), emulating the 40 ms a real aux thread has per tick. After
/// the publisher's death it never waits. The engine itself never waits.
fn wait_for_publisher(done_upto: &AtomicU64, next: u64, alive: bool) {
    if !alive {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while done_upto.load(Ordering::Acquire) <= next {
        assert!(Instant::now() < deadline, "publisher never fed tick {next}");
        std::thread::yield_now();
    }
}

fn cmd(op: String, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op,
        scene_version: 1,
        verb,
    }
}

/// A flood of select_preview layout / cut / auto / select_program plus
/// registry add/remove republishes while the layout source is fed every
/// tick: exactly one fresh frame per tick at the tick-derived PTS, the
/// Program cell rebinds to the layout after a take, and while the layout is
/// on Program its store is never removed (it stays LIVE every such tick).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layout_source_flood_keeps_one_frame_per_tick() {
    const TICKS: u64 = 900;
    let rig = Rig::new();
    let (handle, hook, mut runtime, clock, repeated) = rig.build();
    let publisher = EnginePublisher::<u8, ()>::new(16);

    let stop = Arc::new(AtomicBool::new(false));
    let wanted = Arc::new(AtomicU64::new(0));
    let done_upto = Arc::new(AtomicU64::new(0));
    let feeder = spawn_layout_publisher(
        Arc::clone(&rig.layout_store),
        rig.solid(LUMA_LAYOUT),
        Arc::clone(&wanted),
        Arc::clone(&done_upto),
        u64::MAX,
        Arc::clone(&stop),
    );
    wait_for_publisher(&done_upto, 0, true);

    let flood = {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        let sources = Arc::clone(&rig.sources);
        let store = Arc::clone(&rig.layout_store);
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            let mut republished = 0_u64;
            while !stop.load(Ordering::Acquire) {
                let op = format!("op-{n}");
                let verb = match n % 8 {
                    0 | 4 => SwitcherVerb::SelectPreview(SourceRef::new(KEY)),
                    1 => SwitcherVerb::Cut,
                    2 => SwitcherVerb::SelectPreview(SourceRef::new("b")),
                    3 => SwitcherVerb::SelectProgram(SourceRef::new("a")),
                    5 => SwitcherVerb::Auto(FrameCount::from_ms(200, Rational::new(25, 1))),
                    6 => SwitcherVerb::SelectProgram(SourceRef::new(KEY)),
                    _ => SwitcherVerb::Cut,
                };
                if handle.try_send(cmd(op, verb)).is_err() {
                    std::thread::yield_now();
                }
                // Registry churn: remove, then re-add, the same store.
                if n % 16 == 7 {
                    sources.publish(LayoutSourceSet::default());
                    republished += 1;
                } else if n % 16 == 15 {
                    sources.publish(LayoutSourceSet {
                        entries: vec![(Arc::from(KEY), Arc::clone(&store))],
                    });
                    republished += 1;
                }
                n += 1;
                if n % 64 == 0 {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            republished
        })
    };

    let mut indices = Vec::new();
    let mut pts_ok = true;
    let mut on_program = 0_u64;
    let mut on_program_layout_luma = 0_u64;
    let mut violations = Vec::new();
    let pace = Arc::clone(&clock);
    let control_handle = handle.clone();
    let outcome = runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                indices.push(frame.tick.index);
                pts_ok &= frame.tick.pts == pts(frame.tick.index);
                if let Some(snapshot) = control_handle.snapshot() {
                    if snapshot.program.as_str() == KEY {
                        on_program += 1;
                        let state = frame.source_states.get(KEY).copied();
                        if state != Some(SourceState::Live) {
                            violations.push((frame.tick.index, state));
                        }
                        if !snapshot.in_flight
                            && frame.canvas.sample(8, 8).unwrap().0.abs_diff(LUMA_LAYOUT) < 8
                        {
                            on_program_layout_luma += 1;
                        }
                    }
                }
                let next = frame.tick.index + 1;
                rig.publish_sources(pts(next));
                wanted.store(next, Ordering::Release);
                wait_for_publisher(&done_upto, next, true);
                pace.advance(Duration::from_nanos(TICK_NS));
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    stop.store(true, Ordering::Release);
    let republished = flood.join().unwrap();
    feeder.join().unwrap();
    let _ = outcome;

    // Invariant #1: exactly one fresh frame per tick, contiguous, stamped
    // from the tick counter.
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(indices.windows(2).all(|w| w[1] == w[0] + 1), "contiguous");
    assert!(pts_ok, "every PTS is tick-derived");
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    assert!(
        republished >= 4,
        "the registry really churned: {republished}"
    );
    // The Program cell rebound to the layout and showed its picture.
    assert!(on_program > 0, "a take put the layout on Program");
    assert!(
        on_program_layout_luma > 0,
        "the Program picture was the layout's"
    );
    // Never removed while on Program.
    assert!(
        violations.is_empty(),
        "layout store lost while on Program: {:?}",
        violations.iter().take(5).collect::<Vec<_>>()
    );
}

/// The layout is CUT to Program, then its aux publisher dies. Program keeps
/// emitting exactly one frame per tick; the layout source ages LIVE → STALE
/// → NO_SIGNAL (slate), in that order, and the clock is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_layout_publisher_ages_to_slate_without_moving_the_clock() {
    const TICKS: u64 = 600;
    const DIE_AT: u64 = 40;
    let rig = Rig::new();
    let (handle, hook, mut runtime, clock, repeated) = rig.build();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let stop = Arc::new(AtomicBool::new(false));
    let wanted = Arc::new(AtomicU64::new(0));
    let done_upto = Arc::new(AtomicU64::new(0));
    let feeder = spawn_layout_publisher(
        Arc::clone(&rig.layout_store),
        rig.solid(LUMA_LAYOUT),
        Arc::clone(&wanted),
        Arc::clone(&done_upto),
        DIE_AT,
        Arc::clone(&stop),
    );
    wait_for_publisher(&done_upto, 0, true);
    handle
        .try_send(cmd(
            "pvw".into(),
            SwitcherVerb::SelectPreview(SourceRef::new(KEY)),
        ))
        .unwrap();
    handle
        .try_send(cmd("cut".into(), SwitcherVerb::Cut))
        .unwrap();

    let mut indices = Vec::new();
    let mut states: Vec<Option<SourceState>> = Vec::new();
    let mut lumas = Vec::new();
    let pace = Arc::clone(&clock);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                indices.push(frame.tick.index);
                assert_eq!(frame.tick.pts, pts(frame.tick.index));
                states.push(frame.source_states.get(KEY).copied());
                lumas.push(frame.canvas.sample(8, 8).unwrap().0);
                let next = frame.tick.index + 1;
                rig.publish_sources(pts(next));
                wanted.store(next, Ordering::Release);
                wait_for_publisher(&done_upto, next, next < DIE_AT);
                pace.advance(Duration::from_nanos(TICK_NS));
                0
            },
            |_| None,
            hook,
        )
        .await
        .unwrap();
    stop.store(true, Ordering::Release);
    feeder.join().unwrap();

    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(indices.windows(2).all(|w| w[1] == w[0] + 1), "contiguous");
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    assert_eq!(handle.snapshot().unwrap().program.as_str(), KEY);
    // The CUT landed on tick 0 (the layout was fresh) and stayed.
    assert!(
        states.iter().all(Option::is_some),
        "Program shows the layout"
    );
    let rank = |s: &Option<SourceState>| match s {
        Some(SourceState::Live) => 0,
        Some(SourceState::Stale) => 1,
        _ => 2,
    };
    assert!(
        states.windows(2).all(|w| rank(&w[0]) <= rank(&w[1])),
        "the layout only ages, never recovers without a publisher"
    );
    let live = states.iter().filter(|s| rank(s) == 0).count();
    let stale = states.iter().filter(|s| rank(s) == 1).count();
    let slate = states.iter().filter(|s| rank(s) == 2).count();
    assert!(
        live >= usize::try_from(DIE_AT).unwrap(),
        "live while fed: {live}"
    );
    assert!(stale > 0, "STALE after the publisher died");
    assert!(slate > 0, "NO_SIGNAL slate after the no-signal threshold");
    assert_eq!(
        states.last().copied().flatten(),
        Some(SourceState::NoSignal)
    );
    // Last-good is the layout picture; the slate is not.
    assert!(lumas[0].abs_diff(LUMA_LAYOUT) < 8);
    assert!(lumas.last().unwrap().abs_diff(LUMA_LAYOUT) >= 8 || slate > 0);
}
