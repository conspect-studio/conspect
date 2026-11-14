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
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_precision_loss
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
    TimeSource, TransitionStyle,
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
    let wipe: TransitionDescriptor = serde_json::from_value(
        serde_json::json!({"kind": "dve_push", "direction": "left", "rate": 10}),
    )
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

/// ADR-0112 §4: the published snapshot reports the selection from the tick
/// the command was applied on, never before, and a refused selection leaves
/// it unchanged. An AUTO's own rate is the flight's, never the selection's:
/// the snapshot keeps reporting the operator's selection during and after it.
#[test]
fn the_snapshot_carries_the_selected_transition_after_its_command_tick() {
    let mut rig = rig();
    rig.step(0);
    assert_eq!(
        rig.handle.snapshot().unwrap().transition,
        TransitionDescriptor::Cut
    );
    let selected = wipe(
        12,
        "box",
        &serde_json::json!({"softness": 0.1, "reverse": true, "border_width": 0.02, "border_color": "#ffffff"}),
    );
    rig.send("set", SwitcherVerb::SetTransition(selected.clone()));
    assert_eq!(
        rig.handle.snapshot().unwrap().transition,
        TransitionDescriptor::Cut,
        "queued is not applied"
    );
    rig.step(1);
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.tick, 1);
    assert_eq!(snapshot.transition, selected);
    // A refused selection (not rendered) publishes nothing new.
    rig.send(
        "refused",
        SwitcherVerb::SetTransition(wipe(12, "box", &serde_json::json!({"flip_flop": true}))),
    );
    rig.step(2);
    assert_eq!(rig.handle.snapshot().unwrap().transition, selected);
    // A steady tick republishes nothing (change-only publication).
    let before = rig.handle.snapshot().unwrap();
    rig.step(3);
    assert!(Arc::ptr_eq(&before, &rig.handle.snapshot().unwrap()));
    // The AUTO's rate is the flight's rate; the snapshot keeps the selection.
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()));
    rig.step(4);
    let flight = rig.handle.snapshot().unwrap();
    assert!(flight.in_flight);
    assert_eq!(flight.transition, selected, "the selection, not the flight");
    for index in 5..=9 {
        rig.step(index);
    }
    let done = rig.handle.snapshot().unwrap();
    assert!(!done.in_flight);
    assert_eq!(done.completion_count, flight.completion_count + 1);
    assert_eq!(done.transition, selected, "completion restores nothing: it never changed");
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

/// Counts the ticks composed as a bordered WIPE flight (review INV-01): the
/// drive carries a mix overlay in the wipe style with a border colour.
struct CountWipes<H> {
    inner: H,
    wipes: Arc<AtomicU64>,
}

fn is_bordered_wipe(drive: &CompositorDrive<Nv12Image>) -> bool {
    drive.mix_overlay().is_some()
        && matches!(
            drive.transition_style(),
            TransitionStyle::Wipe {
                border_color: Some(_),
                ..
            }
        )
}

impl<H: FrameControl> FrameControl for CountWipes<H> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.before_compose(drive, tick);
        if is_bordered_wipe(drive) {
            self.wipes.fetch_add(1, Ordering::Release);
        }
    }
    fn after_compose(&mut self, frame: &multiview_engine::CompositedFrame) {
        self.inner.after_compose(frame);
    }
    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.on_repeat(drive, tick);
    }
}

/// The flood's seed (review INV-01): a bordered WIPE and an AUTO queued ahead
/// of every flood command, so a wipe flight certainly runs under the flood.
fn seed_bordered_wipe(handle: &SwitcherControl, rate: u32) {
    for (op, verb) in [
        (
            "seed-wipe",
            SwitcherVerb::SetTransition(wipe(
                rate,
                "circle",
                &serde_json::json!({"softness": 0.05, "border_width": 0.03, "border_color": "#00ff00"}),
            )),
        ),
        (
            "seed-auto",
            SwitcherVerb::Auto(FrameCount::try_from(rate).unwrap()),
        ),
    ] {
        handle
            .try_send(SwitcherCommand {
                operation_id: op.into(),
                scene_version: 1,
                verb,
            })
            .unwrap();
    }
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
    let wipes = Arc::new(AtomicU64::new(0));
    // A 30-frame bordered wipe; the flood starts once its first tick has been
    // composed (a flood CUT drained in the same tick would end it first), so
    // the rest of the flight runs under the flood.
    seed_bordered_wipe(&handle, 30);
    let producer = {
        let handle = handle.clone();
        let (done, accepted, full) = (Arc::clone(&done), Arc::clone(&accepted), Arc::clone(&full));
        let started = Arc::clone(&wipes);
        std::thread::spawn(move || {
            while started.load(Ordering::Acquire) == 0 && !done.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
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
                    // Invalid (a one-frame dip) and unrendered (flip-flop)
                    // selections are refused at the tick, never published.
                    3 if n % 4 == 0 => {
                        SwitcherVerb::SetTransition(dip(1, None, Rational::new(1, 2)))
                    }
                    3 if n % 2 == 0 => SwitcherVerb::SetTransition(wipe(
                        6,
                        "vertical",
                        &serde_json::json!({"flip_flop": true}),
                    )),
                    3 => SwitcherVerb::SetTransition(wipe(
                        6,
                        ["box", "circle", "diagonal"][usize::try_from(n % 3).unwrap()],
                        &serde_json::json!({"softness": 0.1, "border_width": 0.02, "border_color": "#00ff00"}),
                    )),
                    // The WIPE selection (n % 5 == 3, odd n) is followed by an
                    // AUTO; a CUT (which ends any flight) once per 50 commands,
                    // so wipe flights stay on air across flood ticks.
                    _ if n % 50 == 49 => SwitcherVerb::Cut,
                    _ => SwitcherVerb::Auto(FrameCount::try_from(6).unwrap()),
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
            CountWipes {
                inner: rig.hook,
                wipes: Arc::clone(&wipes),
            },
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
    // ADR-0112 §4: under the flood the published selection is always one
    // the engine accepted (never a refused or unrendered descriptor).
    let selection = handle.snapshot().unwrap().transition.clone();
    assert!(
        selection.is_rendered() && selection.is_valid(),
        "{selection:?}"
    );
    assert!(
        !matches!(selection, TransitionDescriptor::Cut),
        "{selection:?}"
    );
    // The seeded bordered wipe was composed on its first tick and again on
    // the next one, after the flood's first 16 commands were drained (the
    // first CUT is command 49, at least three drains later).
    assert!(
        wipes.load(Ordering::Relaxed) >= 2,
        "bordered wipe ticks: {}",
        wipes.load(Ordering::Relaxed)
    );
}

/// The real-time variant of the flood (review INV1-01): a monotonic clock and
/// the real pacer on a dedicated engine thread, watched from this thread by
/// an independent watchdog that samples the engine's tick counter. The
/// outcome stream is never read (a stalled consumer). Bounds are generous
/// counts and gaps (a loaded runner passes), never tight percentiles.
#[test]
fn a_transition_flood_on_a_real_clock_keeps_cadence_with_a_stalled_consumer() {
    let run = flood_on_a_real_clock(Block::None);
    assert!(run.finished, "the engine finished within the deadline");
    assert!(
        run.max_gap < MAX_GAP,
        "the clock never stalled: largest gap between ticks {:?}",
        run.max_gap
    );
    assert!(
        run.repeats <= TICKS_RT / 10,
        "repeats {} of {TICKS_RT}",
        run.repeats
    );
    assert!(run.full > 0, "the flood met a full mailbox");
    assert!(run.completions > 0, "takes really ran");
    assert!(
        run.wipes >= 4,
        "bordered wipe ticks under the flood (seeded 8-frame flight, no CUT; \
         catch-up repeats are not counted): {}",
        run.wipes
    );
}

/// Negative control: the same watchdog with a hook that blocks 3 s once, in
/// the second half of the run, detects the stall, so the gate measures the
/// clock itself. The block fires on the first fresh OR repeat tick at or past
/// its index (a loaded runner may emit any given tick as a catch-up repeat),
/// and the run records that it fired.
#[test]
fn the_real_clock_watchdog_detects_a_long_synchronous_block() {
    let run = flood_on_a_real_clock(Block::Once {
        at: TICKS_RT / 2,
        for_: std::time::Duration::from_secs(3),
    });
    assert!(run.injected, "the block was injected");
    assert!(run.max_gap >= MAX_GAP, "detected: {:?}", run.max_gap);
}

/// Negative control: a hook that blocks 30 ms on every 20 ms tick shows up
/// as catch-up repeats.
#[test]
fn the_real_clock_gate_detects_a_slow_hook() {
    let run = flood_on_a_real_clock(Block::Every(std::time::Duration::from_millis(30)));
    assert!(
        run.repeats > TICKS_RT / 10,
        "a blocking hook must show up as repeats: {}",
        run.repeats
    );
}

const TICKS_RT: u64 = 150; // 3 s at 50 fps
/// The largest tolerated gap between two observed ticks (50 periods).
const MAX_GAP: std::time::Duration = std::time::Duration::from_secs(1);

struct RealRun {
    finished: bool,
    injected: bool,
    max_gap: std::time::Duration,
    repeats: u64,
    full: u64,
    completions: u64,
    wipes: u64,
}

#[derive(Clone, Copy)]
enum Block {
    None,
    Every(std::time::Duration),
    Once { at: u64, for_: std::time::Duration },
}

/// Wraps the switcher hook and blocks the clock as `block` says.
struct Stall<H> {
    inner: H,
    block: Block,
    injected: Arc<AtomicBool>,
    wipes: Arc<AtomicU64>,
}

impl<H> Stall<H> {
    /// Block for `Once` on the first tick at or after `at`, fresh or a
    /// catch-up repeat alike, so a loaded runner that emits the terminal tick
    /// as a repeat still exercises the block (review INV1-01 round 3).
    fn block_once(&mut self, tick: Tick) {
        if let Block::Once { at, for_ } = self.block {
            if tick.index >= at {
                self.block = Block::None;
                self.injected.store(true, Ordering::Release);
                std::thread::sleep(for_);
            }
        }
    }
}

impl<H: FrameControl> FrameControl for Stall<H> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        if let Block::Every(delay) = self.block {
            self.injected.store(true, Ordering::Release);
            std::thread::sleep(delay);
        }
        self.block_once(tick);
        self.inner.before_compose(drive, tick);
        if is_bordered_wipe(drive) {
            self.wipes.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn after_compose(&mut self, frame: &multiview_engine::CompositedFrame) {
        self.inner.after_compose(frame);
    }
    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.block_once(tick);
        self.inner.on_repeat(drive, tick);
    }
}

fn flood_on_a_real_clock(block: Block) -> RealRun {
    use multiview_engine::{MonotonicTimeSource, RealtimePacer};
    // Pacing is measured from before the runtime seeds its first deadline
    // (review INV1-03), so setup time can only lengthen `elapsed`.
    let started = std::time::Instant::now();
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
    let ticks = runtime.ticks_counter();
    let repeated = runtime.frames_repeated_counter();
    // Subscribed and never drained: the engine must not care.
    let _stalled = handle.outcomes();
    let done = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let injected = Arc::new(AtomicBool::new(false));
    let full = Arc::new(AtomicU64::new(0));
    let wipes = Arc::new(AtomicU64::new(0));
    // No CUT in this flood: during the seeded flight every selection and AUTO
    // is refused, so the 8-frame bordered wipe runs under the flood.
    seed_bordered_wipe(&handle, 8);
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
                let verb = match n % 4 {
                    0 => SwitcherVerb::SetTransition(dip(
                        6,
                        Some(Rgb8::new(u8::try_from(n % 251).unwrap(), 0, 0)),
                        Rational::new(1, 3),
                    )),
                    2 if n % 8 == 2 => SwitcherVerb::SetTransition(wipe(
                        6,
                        "circle",
                        &serde_json::json!({"softness": 0.05, "border_width": 0.02, "border_color": "#0000ff"}),
                    )),
                    2 => SwitcherVerb::SetTransition(dip(1, None, Rational::new(1, 2))),
                    // n % 4 == 1 or 3: an AUTO, so every WIPE selection
                    // (n % 8 == 2) is followed by an AUTO.
                    _ => SwitcherVerb::Auto(FrameCount::try_from(6).unwrap()),
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
    // The engine runs on its own thread with its own runtime, so a blocked
    // hook blocks only that thread; the watchdog below keeps running.
    let engine = {
        let finished = Arc::clone(&finished);
        let hook = Stall {
            inner: rig.hook,
            block,
            injected: Arc::clone(&injected),
            wipes: Arc::clone(&wipes),
        };
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let publisher = EnginePublisher::<u8, ()>::new(16);
            let outcome = rt
                .block_on(runtime.run_for_with_frame_control(
                    &publisher,
                    &StopSignal::new(),
                    TICKS_RT,
                    |_| 0,
                    |_| None,
                    hook,
                ))
                .unwrap();
            assert_eq!(outcome.ticks, TICKS_RT);
            finished.store(true, Ordering::Release);
        })
    };
    // Watchdog: sample the tick counter every 10 ms, record the largest gap
    // between progress, stop watching at a hard deadline (3 s of output + 10 s).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(13);
    let mut last = (ticks.load(Ordering::Acquire), std::time::Instant::now());
    let mut max_gap = std::time::Duration::ZERO;
    while !finished.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let now = std::time::Instant::now();
        let seen = ticks.load(Ordering::Acquire);
        if seen != last.0 {
            last = (seen, now);
        }
        max_gap = max_gap.max(now - last.1);
    }
    let finished = finished.load(Ordering::Acquire);
    done.store(true, Ordering::Release);
    producer.join().unwrap();
    feeder.join().unwrap();
    if finished {
        engine.join().unwrap();
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(2900),
            "real pacing: {:?}",
            started.elapsed()
        );
    }
    // A never-finishing engine thread is left detached; the test then fails.
    RealRun {
        finished,
        injected: injected.load(Ordering::Acquire),
        max_gap,
        repeats: repeated.load(Ordering::Acquire),
        full: full.load(Ordering::Relaxed),
        completions: handle.snapshot().map_or(0, |s| s.completion_count),
        wipes: wipes.load(Ordering::Relaxed),
    }
}

fn wipe(rate: u32, pattern: &str, extra: &serde_json::Value) -> TransitionDescriptor {
    let mut value = serde_json::json!({"kind": "wipe", "pattern": pattern, "rate": rate});
    if let (Some(map), Some(more)) = (value.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            map.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(value).unwrap()
}

/// The CPU reference of one wipe frame: outgoing, incoming through the mask,
/// then the optional border band above it (review MED-03).
fn wipe_reference(
    mask: multiview_compositor::mask::WipeMask,
    border: Option<(Rgb8, f32)>,
) -> Nv12Image {
    let a = solid(A_Y);
    let b = solid(B_Y);
    let mut fill = None;
    let mut tiles = vec![Tile::scaled(&a, 0, 0, W, W, 1.0)];
    if let Some((c, _)) = border {
        fill = Some(Nv12Image::solid_rgb(2, 2, c.r, c.g, c.b, CanvasColor::default()).unwrap());
    }
    tiles.push(Tile::scaled(&b, 0, 0, W, W, 1.0).with_mask(mask));
    if let (Some(f), Some((_, width))) = (fill.as_ref(), border) {
        tiles.push(Tile::scaled(f, 0, 0, W, W, 1.0).with_mask(mask.expanded(width)));
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

/// ADR-0112 §3: every pattern, rendered through the switcher, equals the CPU
/// reference composite of the expected masked tile stack at the start, a
/// quarter, half way and the end of a 10-frame AUTO.
#[test]
fn every_wipe_pattern_matches_the_cpu_reference_through_the_switcher() {
    use multiview_compositor::mask::{MaskShape, WipeMask};
    for (pattern, shape) in [
        ("linear_edge", MaskShape::Horizontal),
        ("vertical", MaskShape::Vertical),
        ("diagonal", MaskShape::Diagonal),
        ("box", MaskShape::Box),
        ("circle", MaskShape::Circle),
    ] {
        for (extra, reverse, softness, border) in [
            (serde_json::json!({}), false, 0.0, None),
            (
                serde_json::json!({"reverse": true, "softness": 0.1, "border_width": 0.05, "border_color": "#ffd000"}),
                true,
                0.1,
                Some((Rgb8::new(255, 208, 0), 0.05)),
            ),
        ] {
            let (_, frames) = run_dip(wipe(10, pattern, &extra));
            for (index, elapsed) in [(2_usize, 0_u32), (4, 2), (7, 5)] {
                let mask = WipeMask::new(
                    shape,
                    elapsed as f32 / 10.0,
                    W,
                    W,
                    [0.5, 0.5],
                    reverse,
                    softness,
                    border.map_or(0.0, |(_, w)| w),
                );
                assert_frame(
                    &frames[index],
                    &wipe_reference(mask, border),
                    &format!("{pattern} reverse={reverse} elapsed {elapsed}"),
                );
            }
            assert_frame(&frames[12], &solid_canvas(B_Y), &format!("{pattern}: end"));
        }
    }
}

#[test]
fn out_of_range_or_unrendered_wipe_options_are_refused() {
    let mut rig = rig();
    let mut outcomes = rig.handle.outcomes();
    rig.send(
        "soft",
        SwitcherVerb::SetTransition(wipe(10, "box", &serde_json::json!({"softness": 2.0}))),
    );
    rig.send(
        "origin",
        SwitcherVerb::SetTransition(wipe(
            10,
            "circle",
            &serde_json::json!({"position_xy": [0.5, -1.0]}),
        )),
    );
    rig.send(
        "fill",
        SwitcherVerb::SetTransition(wipe(
            10,
            "box",
            &serde_json::json!({"border_fill_source": "bars"}),
        )),
    );
    rig.send(
        "ff",
        SwitcherVerb::SetTransition(wipe(10, "box", &serde_json::json!({"flip_flop": true}))),
    );
    rig.send(
        "ok",
        SwitcherVerb::SetTransition(wipe(10, "box", &serde_json::json!({}))),
    );
    rig.step(0);
    let mut errors = HashMap::new();
    while let Ok(item) = outcomes.try_recv() {
        errors.insert(item.event.operation_id.clone(), item.event.error.clone());
    }
    for op in ["soft", "origin"] {
        assert!(errors[op].is_some(), "{op}");
    }
    for op in ["fill", "ff"] {
        assert_eq!(
            errors[op].as_deref(),
            Some(REFUSE_UNSUPPORTED_TRANSITION),
            "{op}"
        );
    }
    assert_eq!(errors["ok"], None);
}

/// Review ABU-01: typed wipes carrying NaN, infinities or out-of-range
/// geometry (values JSON cannot carry) are refused and leave the selected
/// transition unchanged.
#[test]
fn malformed_typed_wipes_are_refused_and_change_nothing() {
    let mut rig = rig();
    let mut outcomes = rig.handle.outcomes();
    let good = wipe(
        10,
        "box",
        &serde_json::json!({"border_width": 0.1, "border_color": "#ff0000"}),
    );
    rig.send("good", SwitcherVerb::SetTransition(good.clone()));
    rig.step(0);
    let mut cases = Vec::new();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5, 1.5] {
        for field in 0..4 {
            let mut d = good.clone();
            if let TransitionDescriptor::Wipe {
                softness,
                border_width,
                position_xy,
                ..
            } = &mut d
            {
                match field {
                    0 => *softness = bad,
                    1 => *border_width = bad,
                    2 => position_xy[0] = bad,
                    _ => position_xy[1] = bad,
                }
            }
            cases.push(d);
        }
    }
    let mut index = 1;
    for (n, d) in cases.into_iter().enumerate() {
        rig.send(&format!("bad-{n}"), SwitcherVerb::SetTransition(d));
        rig.step(index);
        index += 1;
        let mut error = None;
        while let Ok(item) = outcomes.try_recv() {
            if item.event.operation_id == format!("bad-{n}") {
                error = item.event.error.clone();
            }
        }
        assert!(error.is_some(), "case {n} refused");
        // An AUTO now must still run the good bordered box wipe.
    }
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(4).unwrap()));
    rig.step(index);
    rig.step(index + 1);
    assert!(
        matches!(
            rig.drive.transition_style(),
            multiview_engine::TransitionStyle::Wipe {
                border_color: Some(c),
                ..
            } if c == Rgb8::new(255, 0, 0)
        ),
        "the good wipe is still selected"
    );
}

/// Review ABU-01 / INV-01: repeated bordered wipes build the border colour
/// once and never grow the scratch pool (the DIP allocation gate, for WIPE).
#[test]
fn repeated_bordered_wipes_build_the_border_once_and_never_grow_the_scratch_pool() {
    let mut rig = rig();
    rig.send(
        "set",
        SwitcherVerb::SetTransition(wipe(
            4,
            "circle",
            &serde_json::json!({"softness": 0.1, "border_width": 0.05, "border_color": "#00ff00"}),
        )),
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

/// Review MED-03: when the incoming source stops being fresh during a
/// bordered soft wipe, Program shows the outgoing picture alone: neither the
/// border colour nor a hard strip across the soft ramp appears.
#[test]
fn a_bordered_wipe_with_a_stale_incoming_source_shows_the_outgoing_picture() {
    let mut rig = rig();
    rig.send(
        "set",
        SwitcherVerb::SetTransition(wipe(
            40,
            "box",
            &serde_json::json!({"softness": 0.1, "border_width": 0.02, "border_color": "#00ff00"}),
        )),
    );
    rig.step(0);
    rig.step(1);
    rig.send(
        "auto",
        SwitcherVerb::Auto(FrameCount::try_from(40).unwrap()),
    );
    rig.step(2);
    // From here only the outgoing source publishes; the incoming one's last
    // fresh frame is tick 2 and it is past the 500 ms hold by tick 22.
    let mut last = None;
    for index in 3..=22 {
        let t = tick(index);
        rig.a.publish(solid(A_Y), t.pts);
        rig.hook.before_compose(&mut rig.drive, t);
        let frame = rig.drive.compose(t).unwrap();
        rig.hook.after_compose(&frame);
        last = Some(Arc::clone(&frame.canvas));
    }
    assert!(
        rig.handle.snapshot().unwrap().in_flight,
        "the wipe is still running"
    );
    assert_frame(
        &last.unwrap(),
        &solid_canvas(A_Y),
        "stale incoming at elapsed 20",
    );
}

/// Review MED-03: a soft wipe whose border is much narrower than the soft
/// edge, with an opaque incoming picture, equals the CPU reference (incoming
/// through the mask, band above it) at several points of the flight.
#[test]
fn a_soft_wipe_with_a_narrow_border_matches_the_reference() {
    use multiview_compositor::mask::{MaskShape, WipeMask};
    let border = Rgb8::new(0, 255, 0);
    let (_, frames) = run_dip(wipe(
        10,
        "circle",
        &serde_json::json!({"softness": 0.2, "border_width": 0.02, "border_color": "#00ff00"}),
    ));
    for (index, elapsed) in [(4_usize, 2_u32), (7, 5), (10, 8)] {
        let mask = WipeMask::new(
            MaskShape::Circle,
            elapsed as f32 / 10.0,
            W,
            W,
            [0.5, 0.5],
            false,
            0.2,
            0.02,
        );
        assert_frame(
            &frames[index],
            &wipe_reference(mask, Some((border, 0.02))),
            &format!("soft narrow border elapsed {elapsed}"),
        );
    }
}
