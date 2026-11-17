//! Chaos for a stored layout as an M/E source (#307 PR 2; invariants #1 and
//! #10). A real `SwitcherFrameHook` over a real `CompositorDrive` adopts a
//! `layout:<id>` store from a `LayoutSources` registry that another thread
//! republishes (add/remove) while a third floods select/cut/auto commands.
//! The layout store itself is fed by a separate "aux" thread at tick pts; in
//! the second test that thread dies (a wedged or panicked routing-aux).
//! Assertions are on counts and bounds only, never wall-clock percentiles.
//!
//! Every run is supervised by an independent watchdog thread (#307 review
//! I2): a run must finish within a generous wall bound, or the watchdog
//! stops the producers and the runtime and the test fails with the last
//! tick reached (an injected stall is shown to trip it). Producers stop on a
//! flag a Drop guard sets, so a panicking assertion never leaves them
//! spinning.
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
use multiview_core::switcher::{FrameCount, SourceRef};
use multiview_core::time::{MediaTime, Rational};
use multiview_core::traits::SourceState;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::SwitcherOutcome;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::switcher_layouts::{LayoutSourceSet, LayoutSources};
use multiview_engine::{aux_tick_channel, AuxTickSender, WithAuxTick, AUX_TICK_CAPACITY};
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// Wall bound of one supervised run (generous: a sanity bound, not a
/// latency budget).
const RUN_LIMIT: Duration = Duration::from_secs(120);

/// Sets the shared stop flag when dropped (normal end or a panic), so no
/// producer thread outlives the test body.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// An independent supervisor of one run: the sink records progress; after
/// `limit` the watchdog raises `expired`, stops the producers and the
/// runtime. If the runtime then still does not return (a clock wedged
/// inside the engine ignores its `StopSignal`), it reports and exits the test
/// process with a failure rather than hang.
struct Watchdog {
    expired: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(limit: Duration, stop: &Arc<AtomicBool>, runtime_stop: &StopSignal) -> Self {
        let expired = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let join = {
            let (expired, finished, progress) = (
                Arc::clone(&expired),
                Arc::clone(&finished),
                Arc::clone(&progress),
            );
            let (stop, runtime_stop) = (Arc::clone(stop), runtime_stop.clone());
            std::thread::spawn(move || {
                let deadline = Instant::now() + limit;
                while Instant::now() < deadline {
                    if finished.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                expired.store(true, Ordering::Release);
                stop.store(true, Ordering::Release);
                runtime_stop.stop();
                let grace = Instant::now() + Duration::from_secs(10);
                while Instant::now() < grace {
                    if finished.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                // The only way to fail a test whose runtime cannot return.
                #[allow(clippy::print_stderr)]
                let () = eprintln!(
                    "watchdog: the run did not return within {limit:?} + 10 s (last tick {}); \
                     the output clock is wedged",
                    progress.load(Ordering::Acquire)
                );
                std::process::exit(101);
            })
        };
        Self {
            expired,
            finished,
            progress,
            join: Some(join),
        }
    }

    fn tick(&self, index: u64) {
        self.progress.store(index, Ordering::Release);
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }

    /// The run returned: stop the watchdog and fail if it had expired.
    fn finish(mut self) {
        self.finished.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
        assert!(
            !self.expired(),
            "watchdog: the run exceeded its wall bound (last tick {})",
            self.progress.load(Ordering::Acquire)
        );
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
    }
}

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
            entries: vec![(Arc::from(KEY), Arc::clone(&self.layout_store), 1)],
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
fn wait_for_publisher(done_upto: &AtomicU64, next: u64, alive: bool, expired: &AtomicBool) {
    if !alive {
        return;
    }
    while done_upto.load(Ordering::Acquire) <= next {
        if expired.load(Ordering::Acquire) {
            return; // the watchdog fired: let the run end, it fails then
        }
        std::thread::yield_now();
    }
}

fn cmd(op: String, verb: SwitcherVerb) -> SwitcherCommand {
    SwitcherCommand {
        operation_id: op,
        scene_version: 1,
        verb,
        expect: None,
    }
}

/// Outcomes of operation ids starting with `prefix`: (applied, refused).
fn tally(outcomes: &[SwitcherOutcome], prefix: &str) -> (u64, u64) {
    let mut applied = 0;
    let mut refused = 0;
    for o in outcomes
        .iter()
        .filter(|o| o.operation_id.starts_with(prefix))
    {
        if o.error.is_none() {
            applied += 1;
        } else {
            refused += 1;
        }
    }
    (applied, refused)
}

fn drain(
    sub: &mut multiview_engine::EventSubscription<SwitcherOutcome>,
    into: &mut Vec<SwitcherOutcome>,
) {
    while let Ok(event) = sub.try_recv() {
        into.push((*event.event).clone());
    }
}

/// Every how many ticks the sink requires the flood to have made progress.
const FLOOD_CHECK_TICKS: u64 = 50;

/// A flood of `select_preview` layout / cut / auto / `select_program` plus
/// registry add/remove republishes while the layout source is fed every
/// tick: exactly one fresh frame per tick at the tick-derived PTS, the
/// Program cell rebinds to the layout after a take, and while the layout is
/// on Program its store is never removed (it stays LIVE every such tick).
/// The flood provably overlaps the run: it starts before tick 0 (a barrier)
/// and the sink requires it to have advanced every
/// [`FLOOD_CHECK_TICKS`] ticks; the counts of applied commands come from
/// the engine's outcomes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layout_source_flood_keeps_one_frame_per_tick() {
    const TICKS: u64 = 900;
    let rig = Rig::new();
    let (handle, hook, mut runtime, clock, repeated) = rig.build();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut outcomes_sub = handle.outcomes();

    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = StopOnDrop(Arc::clone(&stop));
    let runtime_stop = StopSignal::new();
    let watchdog = Watchdog::start(RUN_LIMIT, &stop, &runtime_stop);
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
    wait_for_publisher(&done_upto, 0, true, &watchdog.expired);

    let started = Arc::new(Barrier::new(2));
    let flood_sent = Arc::new(AtomicU64::new(0));
    let flood = {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        let sources = Arc::clone(&rig.sources);
        let store = Arc::clone(&rig.layout_store);
        let started = Arc::clone(&started);
        let flood_sent = Arc::clone(&flood_sent);
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            let mut republished = 0_u64;
            let mut handshake = false;
            while !stop.load(Ordering::Acquire) {
                let op = format!("op-{n}");
                let verb = match n % 8 {
                    0 | 4 => SwitcherVerb::SelectPreview(SourceRef::new(KEY)),
                    1 | 7 => SwitcherVerb::Cut,
                    2 => SwitcherVerb::SelectPreview(SourceRef::new("b")),
                    3 => SwitcherVerb::SelectProgram(SourceRef::new("a")),
                    5 => SwitcherVerb::Auto(FrameCount::from_ms(200, Rational::new(25, 1))),
                    _ => SwitcherVerb::SelectProgram(SourceRef::new(KEY)),
                };
                if handle.try_send(cmd(op, verb)).is_err() {
                    std::thread::yield_now();
                } else {
                    flood_sent.fetch_add(1, Ordering::AcqRel);
                }
                // Registry churn: remove, then re-add, the same store.
                if n % 16 == 7 {
                    sources.publish(LayoutSourceSet::default());
                    republished += 1;
                } else if n % 16 == 15 {
                    sources.publish(LayoutSourceSet {
                        entries: vec![(Arc::from(KEY), Arc::clone(&store), 1)],
                    });
                    republished += 1;
                }
                n += 1;
                if !handshake && republished >= 2 && flood_sent.load(Ordering::Acquire) > 0 {
                    // Provably running (a command queued, the registry
                    // churned) before the measured window opens.
                    started.wait();
                    handshake = true;
                }
                if n % 64 == 0 {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            if !handshake {
                started.wait();
            }
            republished
        })
    };
    started.wait();

    let mut indices = Vec::new();
    let mut pts_ok = true;
    let mut on_program = 0_u64;
    let mut on_program_layout_luma = 0_u64;
    let mut violations = Vec::new();
    let mut outcomes = Vec::new();
    let mut flood_stalls = 0_u64;
    let mut last_flood = 0_u64;
    let pace = Arc::clone(&clock);
    let control_handle = handle.clone();
    let expired = Arc::clone(&watchdog.expired);
    let outcome = runtime
        .run_for_with_frame_control(
            &publisher,
            &runtime_stop,
            TICKS,
            |frame| {
                watchdog.tick(frame.tick.index);
                indices.push(frame.tick.index);
                pts_ok &= frame.tick.pts == pts(frame.tick.index);
                drain(&mut outcomes_sub, &mut outcomes);
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
                // The flood keeps running through the window: by every
                // check point it queued more since the previous one (wait,
                // bounded by the watchdog, never by a timing guess).
                if frame.tick.index % FLOOD_CHECK_TICKS == FLOOD_CHECK_TICKS - 1 {
                    while flood_sent.load(Ordering::Acquire) <= last_flood
                        && !expired.load(Ordering::Acquire)
                    {
                        std::thread::yield_now();
                    }
                    let now = flood_sent.load(Ordering::Acquire);
                    if now <= last_flood {
                        flood_stalls += 1;
                    }
                    last_flood = now;
                }
                let next = frame.tick.index + 1;
                rig.publish_sources(pts(next));
                wanted.store(next, Ordering::Release);
                wait_for_publisher(&done_upto, next, true, &expired);
                pace.advance(Duration::from_nanos(TICK_NS));
                0
            },
            |_| None,
            hook,
        )
        .await;
    stop.store(true, Ordering::Release);
    let republished = flood.join().unwrap();
    feeder.join().unwrap();
    watchdog.finish();
    let _ = outcome.unwrap();
    drain(&mut outcomes_sub, &mut outcomes);

    // Invariant #1: exactly one fresh frame per tick, contiguous, stamped
    // from the tick counter.
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(indices.windows(2).all(|w| w[1] == w[0] + 1), "contiguous");
    assert!(pts_ok, "every PTS is tick-derived");
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    // The flood overlapped the whole window and really executed.
    assert_eq!(flood_stalls, 0, "the flood advanced at every check point");
    assert!(
        flood_sent.load(Ordering::Acquire) >= TICKS / FLOOD_CHECK_TICKS,
        "flood commands accepted: {}",
        flood_sent.load(Ordering::Acquire)
    );
    assert!(
        republished >= 2,
        "the registry really churned: {republished}"
    );
    let (applied, refused) = tally(&outcomes, "op-");
    // Commands are drained at <= 16 per tick; every accepted one gets an
    // outcome (applied or typed refusal) except those still queued or held
    // at the end, and the engine applied a real share of them.
    assert!(applied + refused > 0, "the flood reached the engine");
    assert!(
        applied >= TICKS / FLOOD_CHECK_TICKS,
        "applied flood commands: {applied} (refused {refused})"
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

/// #307 review I2: the watchdog is a real gate. A sink that stalls (an
/// injected clock stall) trips it: the run is stopped and the stall is
/// reported, rather than passing or hanging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_injected_clock_stall_trips_the_watchdog() {
    let rig = Rig::new();
    let (_handle, hook, mut runtime, clock, _repeated) = rig.build();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = StopOnDrop(Arc::clone(&stop));
    let runtime_stop = StopSignal::new();
    let watchdog = Watchdog::start(Duration::from_millis(300), &stop, &runtime_stop);
    let expired = Arc::clone(&watchdog.expired);
    let pace = Arc::clone(&clock);
    let mut emitted = 0_u64;
    let _ = runtime
        .run_for_with_frame_control(
            &publisher,
            &runtime_stop,
            1_000,
            |frame| {
                watchdog.tick(frame.tick.index);
                emitted += 1;
                if frame.tick.index == 5 {
                    // The stall: the tick never completes until the
                    // watchdog fires.
                    while !expired.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                rig.publish_sources(pts(frame.tick.index + 1));
                pace.advance(Duration::from_nanos(TICK_NS));
                0
            },
            |_| None,
            hook,
        )
        .await;
    assert!(emitted < 1_000, "the stalled run was stopped: {emitted}");
    let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watchdog.finish()));
    let message = finished
        .expect_err("an expired watchdog fails the run")
        .downcast::<String>()
        .unwrap();
    assert!(message.contains("exceeded its wall bound"), "{message}");
}

/// The layout is CUT to Program, then its aux publisher dies. Program keeps
/// emitting exactly one frame per tick; the layout source ages LIVE → STALE
/// → `NO_SIGNAL` (slate), in that order, and the clock is unaffected. The
/// final Program picture is the no-signal slate itself (#307 review M4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_layout_publisher_ages_to_slate_without_moving_the_clock() {
    const TICKS: u64 = 600;
    const DIE_AT: u64 = 40;
    let rig = Rig::new();
    let (handle, hook, mut runtime, clock, repeated) = rig.build();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = StopOnDrop(Arc::clone(&stop));
    let runtime_stop = StopSignal::new();
    let watchdog = Watchdog::start(RUN_LIMIT, &stop, &runtime_stop);
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
    wait_for_publisher(&done_upto, 0, true, &watchdog.expired);
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
    let mut last_canvas = None;
    let pace = Arc::clone(&clock);
    let expired = Arc::clone(&watchdog.expired);
    let result = runtime
        .run_for_with_frame_control(
            &publisher,
            &runtime_stop,
            TICKS,
            |frame| {
                watchdog.tick(frame.tick.index);
                indices.push(frame.tick.index);
                assert_eq!(frame.tick.pts, pts(frame.tick.index));
                states.push(frame.source_states.get(KEY).copied());
                lumas.push(frame.canvas.sample(8, 8).unwrap().0);
                last_canvas = Some(Arc::clone(&frame.canvas));
                let next = frame.tick.index + 1;
                rig.publish_sources(pts(next));
                wanted.store(next, Ordering::Release);
                wait_for_publisher(&done_upto, next, next < DIE_AT, &expired);
                pace.advance(Duration::from_nanos(TICK_NS));
                0
            },
            |_| None,
            hook,
        )
        .await;
    stop.store(true, Ordering::Release);
    feeder.join().unwrap();
    watchdog.finish();
    result.unwrap();

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
    // Last-good is the layout picture.
    assert!(lumas[0].abs_diff(LUMA_LAYOUT) < 8);
    // The final Program picture IS the no-signal slate (the drive's card,
    // solid luma SLATE_LUMA), independent of the state counts: every pixel.
    let last = last_canvas.unwrap();
    assert!(
        last.y_plane().iter().all(|&y| y == SLATE_LUMA),
        "the final Program picture is the slate (luma {SLATE_LUMA}), first pixel {}",
        last.y_plane()[0]
    );
    assert_ne!(SLATE_LUMA, LUMA_LAYOUT, "the slate is not last-good");
}

/// The drive's no-signal card luma ([`Rig::build`] passes `solid(SLATE_LUMA)`).
const SLATE_LUMA: u8 = 16;

/// #307 review I2: the M/E-only aux gate (`me_layout_flag` raised, routed
/// flag false) with (a) a receiver that is never drained (the channel fills)
/// and (b) a dropped receiver. The clock still emits exactly one frame per
/// tick with a tick-derived PTS, `sent` stops at the channel capacity (a)
/// or zero (b), and `dropped` takes every other fresh tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_me_only_aux_gate_never_moves_the_clock_when_aux_is_wedged_or_gone() {
    const TICKS: u64 = 300;
    for dropped_receiver in [false, true] {
        let rig = Rig::new();
        rig.layout_store.publish(rig.solid(LUMA_LAYOUT), pts(0));
        let (_handle, hook, mut runtime, clock, repeated) = rig.build();
        let publisher = EnginePublisher::<u8, ()>::new(16);
        let (tx, rx) = aux_tick_channel();
        let tx: AuxTickSender = tx;
        tx.me_layout_flag().store(true, Ordering::Release);
        assert!(!tx.active_flag().load(Ordering::Acquire), "routed gate off");
        let (sent, dropped) = (tx.sent_counter(), tx.dropped_counter());
        let wedged = if dropped_receiver {
            drop(rx);
            None
        } else {
            Some(rx) // held, never read
        };
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_guard = StopOnDrop(Arc::clone(&stop));
        let runtime_stop = StopSignal::new();
        let watchdog = Watchdog::start(RUN_LIMIT, &stop, &runtime_stop);
        let mut indices = Vec::new();
        let mut pts_ok = true;
        let pace = Arc::clone(&clock);
        let result = runtime
            .run_for_with_frame_control(
                &publisher,
                &runtime_stop,
                TICKS,
                |frame| {
                    watchdog.tick(frame.tick.index);
                    indices.push(frame.tick.index);
                    pts_ok &= frame.tick.pts == pts(frame.tick.index);
                    rig.publish_sources(pts(frame.tick.index + 1));
                    pace.advance(Duration::from_nanos(TICK_NS));
                    0
                },
                |_| None,
                WithAuxTick {
                    inner: hook,
                    aux: tx,
                },
            )
            .await;
        watchdog.finish();
        result.unwrap();
        drop(wedged);
        assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
        assert!(indices.windows(2).all(|w| w[1] == w[0] + 1), "contiguous");
        assert!(pts_ok, "every PTS is tick-derived");
        assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
        let capacity = u64::try_from(AUX_TICK_CAPACITY).unwrap();
        let (sent, dropped) = (
            sent.load(Ordering::Acquire),
            dropped.load(Ordering::Acquire),
        );
        if dropped_receiver {
            assert_eq!((sent, dropped), (0, TICKS), "gone: every tick dropped");
        } else {
            assert_eq!(sent, capacity, "wedged: the channel holds its capacity");
            assert_eq!(dropped, TICKS - capacity, "wedged: the rest dropped");
        }
    }
}

/// #307 review M4: AUTO and DIP with `layout:wall` on Preview complete after
/// exactly N ticks on a manual clock, and Program then shows the layout's
/// pixels.
#[test]
fn auto_and_dip_of_a_layout_complete_after_exactly_n_ticks() {
    use multiview_core::switcher::TransitionDescriptor;
    use multiview_engine::clock::Tick;
    use multiview_engine::runtime::FrameControl;
    const N: u32 = 6;
    for dip in [false, true] {
        let rig = Rig::new();
        let (handle, mut hook, _runtime, _clock, _repeated) = rig.build();
        let mut drive = CompositorDrive::new(
            layout("a"),
            HashMap::from([
                ("a".to_owned(), Arc::clone(&rig.a)),
                ("b".to_owned(), Arc::clone(&rig.b)),
            ]),
            rig.solid(SLATE_LUMA),
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
        )
        .unwrap()
        .with_cell_ids(vec![Some("pgm".into())]);
        let mut sub = handle.outcomes();
        let mut outcomes = Vec::new();
        let mut step = |index: u64| {
            let tick = Tick {
                index,
                pts: pts(index),
            };
            rig.publish_sources(tick.pts);
            rig.layout_store.publish(rig.solid(LUMA_LAYOUT), tick.pts);
            hook.before_compose(&mut drive, tick);
            let frame = drive.compose(tick).unwrap();
            hook.after_compose(&frame);
            frame
        };
        if dip {
            let descriptor: TransitionDescriptor = serde_json::from_value(serde_json::json!({
                "kind": "dip", "rate": N, "switch_point": {"num": 1, "den": 2},
            }))
            .unwrap();
            handle
                .try_send(cmd("set".into(), SwitcherVerb::SetTransition(descriptor)))
                .unwrap();
        }
        handle
            .try_send(cmd(
                "pvw".into(),
                SwitcherVerb::SelectPreview(SourceRef::new(KEY)),
            ))
            .unwrap();
        step(0);
        assert_eq!(handle.snapshot().unwrap().preview.as_str(), KEY);
        handle
            .try_send(cmd(
                "auto".into(),
                SwitcherVerb::Auto(FrameCount::try_from(N).unwrap()),
            ))
            .unwrap();
        // The AUTO starts on tick 1 and completes on tick 1 + N.
        let start = 1_u64;
        for index in start..start + u64::from(N) {
            let frame = step(index);
            drain(&mut sub, &mut outcomes);
            assert!(
                !outcomes.iter().any(|o| o.operation_id == "auto"),
                "{}: no outcome before tick {} (at {index})",
                if dip { "dip" } else { "mix" },
                start + u64::from(N)
            );
            assert!(handle.snapshot().unwrap().in_flight);
            let _ = frame;
        }
        let frame = step(start + u64::from(N));
        drain(&mut sub, &mut outcomes);
        let auto = outcomes
            .iter()
            .find(|o| o.operation_id == "auto")
            .expect("the take completed");
        assert_eq!(auto.error, None);
        assert!(auto.completed);
        assert_eq!(auto.tick, start + u64::from(N), "exactly N ticks");
        let snapshot = handle.snapshot().unwrap();
        assert_eq!(snapshot.program.as_str(), KEY);
        assert!(!snapshot.in_flight);
        // Program shows the layout's pixels, everywhere.
        assert!(
            frame.canvas.y_plane().iter().all(|&y| y == LUMA_LAYOUT),
            "Program is the layout picture after the take"
        );
    }
}
