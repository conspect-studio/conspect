//! ADR-0112 §2: DIP to a colour, rendered inside the one Program drive.
//!
//! Deterministic per-frame proof: the composited Program frame at the start,
//! mid-way to the switch point, at the switch point, after it and at the end
//! equals the CPU reference composite of the expected tile stack (outgoing,
//! colour at the dip weight, incoming at its weight), byte for byte. Plus the
//! refusals, the allocation gate and a command flood against the clock
//! (invariants #1 and #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{self, CanvasColor, Nv12Image, Tile};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, Rgb8, SourceRef, TransitionDescriptor};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::{ManualTimeSource, Tick};
use multiview_engine::runtime::FrameControl;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb, COMMAND_CAPACITY,
    MAX_COMMANDS_PER_TICK, REFUSE_UNSUPPORTED_TRANSITION,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::TileStore;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const W: u32 = 64;
const TICK_NS: i64 = 40_000_000;
const A_Y: u8 = 200;
const B_Y: u8 = 60;

fn color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(W, W)
}

fn layout(id: &str) -> Arc<Layout> {
    Arc::new(Layout {
        name: "program".into(),
        canvas: Canvas {
            width: W,
            height: W,
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
    Nv12Image::solid(W, W, y, 128, 128, color()).unwrap()
}

fn tick(index: u64) -> Tick {
    Tick {
        index,
        pts: MediaTime::from_nanos(i64::try_from(index).unwrap() * TICK_NS),
    }
}

struct Rig {
    a: Arc<TileStore<Nv12Image>>,
    b: Arc<TileStore<Nv12Image>>,
    handle: SwitcherControl,
    hook: SwitcherFrameHook<fn(&mut CompositorDrive<Nv12Image>)>,
    drive: CompositorDrive<Nv12Image>,
}

fn noop(_: &mut CompositorDrive<Nv12Image>) {}

fn rig() -> Rig {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    let stores = HashMap::from([("a".into(), Arc::clone(&a)), ("b".into(), Arc::clone(&b))]);
    let drive = CompositorDrive::new(
        layout("a"),
        stores.clone(),
        solid(16),
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
    let hook = SwitcherFrameHook::new(driver, "pgm".into(), {
        let hook: fn(&mut CompositorDrive<Nv12Image>) = noop;
        hook
    });
    Rig {
        a,
        b,
        handle,
        hook,
        drive,
    }
}

impl Rig {
    fn send(&self, op: &str, verb: SwitcherVerb) {
        self.handle
            .try_send(SwitcherCommand {
                operation_id: op.into(),
                scene_version: 1,
                verb,
            })
            .unwrap();
    }

    /// One fresh output tick: both sources publish a frame stamped at it.
    fn step(&mut self, index: u64) -> Arc<Nv12Image> {
        let t = tick(index);
        self.a.publish(solid(A_Y), t.pts);
        self.b.publish(solid(B_Y), t.pts);
        self.hook.before_compose(&mut self.drive, t);
        let frame = self.drive.compose(t).unwrap();
        self.hook.after_compose(&frame);
        Arc::clone(&frame.canvas)
    }
}

fn dip(rate: u32, color: Option<Rgb8>, switch_point: Rational) -> TransitionDescriptor {
    serde_json::from_value(serde_json::json!({
        "kind": "dip",
        "rate": rate,
        "switch_point": {"num": switch_point.num, "den": switch_point.den},
        "color": color.map(|c| c.to_string()),
    }))
    .unwrap()
}

/// The CPU reference: outgoing at 1, colour at `dip`, incoming at `incoming`.
fn reference(fill: Rgb8, dip: f32, incoming: f32) -> Nv12Image {
    let a = solid(A_Y);
    let b = solid(B_Y);
    let c = Nv12Image::solid_rgb(2, 2, fill.r, fill.g, fill.b, CanvasColor::default()).unwrap();
    let mut tiles = vec![Tile::scaled(&a, 0, 0, W, W, 1.0)];
    if dip > 0.0 {
        tiles.push(Tile::scaled(&c, 0, 0, W, W, dip));
    }
    if incoming > 0.0 {
        tiles.push(Tile::scaled(&b, 0, 0, W, W, incoming));
    }
    pipeline::composite(
        W,
        W,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    )
    .unwrap()
}

fn assert_frame(got: &Nv12Image, want: &Nv12Image, what: &str) {
    assert_eq!(got.y_plane(), want.y_plane(), "{what}: luma");
    assert_eq!(got.uv_plane(), want.uv_plane(), "{what}: chroma");
}

/// Run one 10-frame dip that starts on tick 2; return the frames by tick.
fn run_dip(descriptor: TransitionDescriptor) -> (Rig, Vec<Arc<Nv12Image>>) {
    let mut rig = rig();
    rig.send("set", SwitcherVerb::SetTransition(descriptor));
    let mut frames = vec![rig.step(0), rig.step(1)];
    rig.send(
        "auto",
        SwitcherVerb::Auto(FrameCount::try_from(10).unwrap()),
    );
    for index in 2..16 {
        frames.push(rig.step(index));
    }
    (rig, frames)
}

#[test]
fn dip_to_black_matches_the_cpu_reference_at_start_mid_switch_and_end() {
    let (rig, frames) = run_dip(dip(10, None, Rational::new(1, 2)));
    let black = Rgb8::BLACK;
    // The AUTO starts on tick 2 (elapsed 0): outgoing only.
    assert_frame(&frames[2], &reference(black, 0.0, 0.0), "start");
    // Elapsed 2 of 10, switch at 5: colour at 2/5 over the outgoing picture.
    assert_frame(&frames[4], &reference(black, 0.4, 0.0), "mid-out");
    // Elapsed 5 = the switch point: the colour alone.
    assert_frame(&frames[7], &reference(black, 1.0, 0.0), "switch point");
    // Elapsed 8: the incoming picture at 3/5 over the colour.
    assert_frame(&frames[10], &reference(black, 1.0, 0.6), "mid-in");
    // Elapsed 10 = terminal tick: the take completed, incoming only.
    assert_frame(&frames[12], &solid_canvas(B_Y), "end");
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert!(!snapshot.in_flight);
    // Luma falls monotonically to black and rises monotonically to B.
    let luma: Vec<u8> = frames[2..=12]
        .iter()
        .map(|f| f.sample(8, 8).unwrap().0)
        .collect();
    assert!(luma[..=5].windows(2).all(|w| w[1] < w[0]), "{luma:?}");
    assert_eq!(luma[5], 16, "black at the switch point: {luma:?}");
    assert!(luma[5..].windows(2).all(|w| w[1] > w[0]), "{luma:?}");
}

/// The composite of one opaque full-canvas solid, as the drive renders it.
fn solid_canvas(y: u8) -> Nv12Image {
    let image = solid(y);
    pipeline::composite(
        W,
        W,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &[Tile::scaled(&image, 0, 0, W, W, 1.0)],
    )
    .unwrap()
}

#[test]
fn dip_to_a_colour_with_an_early_switch_point_matches_the_reference() {
    let red = Rgb8::parse("#c81e1e").unwrap();
    let (_, frames) = run_dip(dip(10, Some(red), Rational::new(1, 5)));
    // Switch at 2 of 10.
    assert_frame(&frames[3], &reference(red, 0.5, 0.0), "half way to red");
    assert_frame(&frames[4], &reference(red, 1.0, 0.0), "red");
    assert_frame(&frames[8], &reference(red, 1.0, 0.5), "half way up");
    let (_, cb, cr) = frames[4].sample(8, 8).unwrap();
    assert!(
        cr > 160 && cb < 128,
        "the switch point is red: cb {cb} cr {cr}"
    );
}

#[test]
fn unrendered_or_invalid_transitions_are_refused_and_change_nothing() {
    let mut rig = rig();
    let mut outcomes = rig.handle.outcomes();
    let through_source: TransitionDescriptor = serde_json::from_value(
        serde_json::json!({"kind": "dip", "rate": 10, "dip_source": "bars"}),
    )
    .unwrap();
    let wipe: TransitionDescriptor =
        serde_json::from_value(serde_json::json!({"kind": "wipe", "pattern": "box", "rate": 10}))
            .unwrap();
    rig.send("src", SwitcherVerb::SetTransition(through_source));
    rig.send("wipe", SwitcherVerb::SetTransition(wipe));
    // A one-frame dip has no switch point (ADR-0055 §3).
    rig.send(
        "short",
        SwitcherVerb::SetTransition(dip(1, None, Rational::new(1, 2))),
    );
    rig.send(
        "edge",
        SwitcherVerb::SetTransition(dip(10, None, Rational::new(1, 1))),
    );
    rig.step(0);
    let mut errors = HashMap::new();
    while let Ok(item) = outcomes.try_recv() {
        errors.insert(item.event.operation_id.clone(), item.event.error.clone());
    }
    assert_eq!(
        errors["src"].as_deref(),
        Some(REFUSE_UNSUPPORTED_TRANSITION)
    );
    assert_eq!(
        errors["wipe"].as_deref(),
        Some(REFUSE_UNSUPPORTED_TRANSITION)
    );
    assert!(errors["short"].is_some());
    assert!(errors["edge"].is_some());
    // Nothing was selected: an AUTO is still a dissolve.
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(4).unwrap()));
    rig.step(1);
    let mid = rig.step(3);
    let y = mid.sample(8, 8).unwrap().0;
    assert!(
        y > B_Y && y < A_Y,
        "a dissolve never passes through black: {y}"
    );
}

#[test]
fn a_transition_change_during_a_take_is_refused_and_the_flight_keeps_its_dip() {
    let mut rig = rig();
    let mut outcomes = rig.handle.outcomes();
    rig.send(
        "set",
        SwitcherVerb::SetTransition(dip(10, None, Rational::new(1, 2))),
    );
    rig.step(0);
    rig.send(
        "auto",
        SwitcherVerb::Auto(FrameCount::try_from(10).unwrap()),
    );
    rig.step(1);
    rig.send(
        "mid",
        SwitcherVerb::SetTransition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(10).unwrap(),
        }),
    );
    rig.step(2);
    let at_switch = rig.step(6);
    assert_eq!(at_switch.sample(8, 8).unwrap().0, 16, "still dips to black");
    let mut refused = None;
    while let Ok(item) = outcomes.try_recv() {
        if item.event.operation_id == "mid" {
            refused = item.event.error.clone();
        }
    }
    assert!(refused.unwrap().contains("active take"));
}

/// Invariant #1 allocation gate: the colour is coded once per distinct colour,
/// and repeated dips grow neither the fill odometer nor the scratch pool.
#[test]
fn repeated_dips_build_the_colour_once_and_never_grow_the_scratch_pool() {
    let mut rig = rig();
    rig.send(
        "set",
        SwitcherVerb::SetTransition(dip(4, None, Rational::new(1, 2))),
    );
    rig.step(0);
    let mut index = 1;
    let mut reservations = None;
    for round in 0..5 {
        rig.send(
            &format!("auto-{round}"),
            SwitcherVerb::Auto(FrameCount::try_from(4).unwrap()),
        );
        for _ in 0..6 {
            rig.step(index);
            index += 1;
        }
        let now = rig.drive.scratch_reservations();
        if let Some(before) = reservations {
            assert_eq!(now, before, "round {round}: the scratch pool did not grow");
        }
        reservations = Some(now);
    }
    assert_eq!(rig.drive.transition_fill_builds(), 1);
    assert_eq!(rig.handle.snapshot().unwrap().completion_count, 5);
}

/// A flood of transition selections and AUTOs (valid, invalid, unsupported)
/// from another thread while the real runtime emits 600 ticks: exactly one
/// frame per tick, no repeat, the producer meets a full mailbox instead of
/// blocking, at most 16 commands per tick, one outcome per accepted command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transition_command_flood_never_moves_the_output_clock() {
    const TICKS: u64 = 600;
    let rig = rig();
    let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
    let handle = rig.handle.clone();
    let clock = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = clock.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(25, 1)).unwrap(),
        rig.drive,
        ts,
        CooperativePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    let mut outcomes = handle.outcomes();
    let done = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let full = Arc::new(AtomicU64::new(0));
    let producer = {
        let handle = handle.clone();
        let (done, accepted, full) = (Arc::clone(&done), Arc::clone(&accepted), Arc::clone(&full));
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while !done.load(Ordering::Acquire) {
                let op = format!("op-{n}");
                let shade = u8::try_from(n % 7).unwrap() * 30;
                let verb = match n % 5 {
                    0 => SwitcherVerb::SetTransition(dip(
                        6,
                        Some(Rgb8::new(shade, 0, 0)),
                        Rational::new(1, 2),
                    )),
                    1 => SwitcherVerb::Auto(FrameCount::try_from(6).unwrap()),
                    2 => SwitcherVerb::SetTransition(TransitionDescriptor::Mix {
                        rate: FrameCount::try_from(6).unwrap(),
                    }),
                    3 => SwitcherVerb::SetTransition(dip(1, None, Rational::new(1, 2))),
                    _ => SwitcherVerb::Cut,
                };
                let command = SwitcherCommand {
                    operation_id: op.clone(),
                    scene_version: 1,
                    verb,
                };
                if handle.try_send(command).is_ok() {
                    accepted.lock().unwrap().push(op);
                } else {
                    full.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                n += 1;
            }
        })
    };
    let mut indices = Vec::new();
    let mut seen = HashSet::new();
    let mut per_tick: HashMap<u64, usize> = HashMap::new();
    let mut lagged = 0_u64;
    let pace = Arc::clone(&clock);
    a.publish(solid(A_Y), MediaTime::ZERO);
    b.publish(solid(B_Y), MediaTime::ZERO);
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                indices.push(frame.tick.index);
                loop {
                    match outcomes.try_recv() {
                        Ok(item) => {
                            assert!(seen.insert(item.event.operation_id.clone()));
                            *per_tick.entry(item.event.tick).or_default() += 1;
                        }
                        Err(multiview_engine::isolation::TryRecvError::Lagged(n)) => lagged += n,
                        Err(_) => break,
                    }
                }
                pace.advance(std::time::Duration::from_millis(40));
                let next =
                    MediaTime::from_nanos(i64::try_from(frame.tick.index + 1).unwrap() * TICK_NS);
                a.publish(solid(A_Y), next);
                b.publish(solid(B_Y), next);
                0
            },
            |_| None,
            rig.hook,
        )
        .await
        .unwrap();
    done.store(true, Ordering::Release);
    producer.join().unwrap();
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    assert!(
        full.load(Ordering::Relaxed) > 0,
        "the flood met a full mailbox"
    );
    assert_eq!(lagged, 0);
    let accepted = accepted.lock().unwrap().clone();
    let unresolved = accepted.iter().filter(|op| !seen.contains(*op)).count();
    assert!(
        unresolved <= COMMAND_CAPACITY + 1,
        "unresolved {unresolved}"
    );
    assert!(
        per_tick.values().all(|&n| n <= MAX_COMMANDS_PER_TICK + 1),
        "bounded work per tick: {:?}",
        per_tick.values().max()
    );
    assert!(u64::try_from(seen.len()).unwrap() >= TICKS);
}

/// The real-time variant of the flood (review INV1-01): a monotonic clock and
/// the real pacer, so time keeps moving while commands drain and dips render.
/// The outcome stream is never read (a stalled consumer). A stall on the clock
/// shows up as catch-up repeats and as an overrun of the external deadline;
/// both are bounded by the design (cadence-hold), not by tight percentiles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transition_flood_on_a_real_clock_keeps_cadence_with_a_stalled_consumer() {
    let run = flood_on_a_real_clock(std::time::Duration::ZERO).await;
    // A loaded runner may cost a few catch-up repeats, never most of them.
    assert!(
        run.repeats <= TICKS_RT / 10,
        "repeats {} of {TICKS_RT}",
        run.repeats
    );
    assert!(run.full > 0, "the flood met a full mailbox");
    assert!(run.completions > 0, "dips really ran");
}

/// Negative control: the same gate with a hook that blocks 30 ms a tick (a
/// 20 ms period) fails it, so the gate measures the clock, not the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_clock_gate_detects_a_blocking_hook() {
    let run = flood_on_a_real_clock(std::time::Duration::from_millis(30)).await;
    assert!(
        run.repeats > TICKS_RT / 10,
        "a blocking hook must show up as repeats: {}",
        run.repeats
    );
}

const TICKS_RT: u64 = 150; // 3 s at 50 fps

struct RealRun {
    repeats: u64,
    full: u64,
    completions: u64,
}

/// Wraps the switcher hook and optionally blocks the clock for `delay`.
struct Stall<H> {
    inner: H,
    delay: std::time::Duration,
}

impl<H: FrameControl> FrameControl for Stall<H> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.inner.before_compose(drive, tick);
    }
    fn after_compose(&mut self, frame: &multiview_engine::CompositedFrame) {
        self.inner.after_compose(frame);
    }
    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.on_repeat(drive, tick);
    }
}

async fn flood_on_a_real_clock(delay: std::time::Duration) -> RealRun {
    use multiview_engine::{MonotonicTimeSource, RealtimePacer};
    const TICKS: u64 = TICKS_RT;
    let rig = rig();
    let (a, b) = (Arc::clone(&rig.a), Arc::clone(&rig.b));
    let handle = rig.handle.clone();
    let time = Arc::new(MonotonicTimeSource::new());
    let ts: Arc<dyn TimeSource> = time.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(Rational::new(50, 1)).unwrap(),
        rig.drive,
        ts,
        RealtimePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher = EnginePublisher::<u8, ()>::new(16);
    // Subscribed and never drained: the engine must not care.
    let _stalled = handle.outcomes();
    let done = Arc::new(AtomicBool::new(false));
    let full = Arc::new(AtomicU64::new(0));
    // Sources publish fresh frames on real time, off the clock.
    let feeder = {
        let (done, time) = (Arc::clone(&done), Arc::clone(&time));
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let now = MediaTime::from_nanos(time.now_nanos());
                a.publish(solid(A_Y), now);
                b.publish(solid(B_Y), now);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let producer = {
        let (handle, done, full) = (handle.clone(), Arc::clone(&done), Arc::clone(&full));
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while !done.load(Ordering::Acquire) {
                let verb = match n % 3 {
                    0 => SwitcherVerb::SetTransition(dip(
                        6,
                        Some(Rgb8::new(u8::try_from(n % 251).unwrap(), 0, 0)),
                        Rational::new(1, 3),
                    )),
                    1 => SwitcherVerb::Auto(FrameCount::try_from(6).unwrap()),
                    _ => SwitcherVerb::SetTransition(dip(1, None, Rational::new(1, 2))),
                };
                let command = SwitcherCommand {
                    operation_id: format!("rt-{n}"),
                    scene_version: 1,
                    verb,
                };
                if handle.try_send(command).is_err() {
                    full.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                n += 1;
            }
        })
    };
    let started = std::time::Instant::now();
    let mut indices = Vec::new();
    let stop = StopSignal::new();
    let run = runtime.run_for_with_frame_control(
        &publisher,
        &stop,
        TICKS,
        |frame| {
            indices.push(frame.tick.index);
            0
        },
        |_| None,
        Stall {
            inner: rig.hook,
            delay,
        },
    );
    // External deadline: 3 s of output may not take more than 3 s + 10 s
    // (the blocking control takes 150 x 30 ms = 4.5 s).
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(13), run)
        .await
        .expect("the output clock kept running under the flood")
        .unwrap();
    let elapsed = started.elapsed();
    done.store(true, Ordering::Release);
    producer.join().unwrap();
    feeder.join().unwrap();
    assert_eq!(outcome.ticks, TICKS);
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(2900),
        "real pacing: {elapsed:?}"
    );
    RealRun {
        repeats: repeated.load(Ordering::Acquire),
        full: full.load(Ordering::Relaxed),
        completions: handle.snapshot().map_or(0, |s| s.completion_count),
    }
}
