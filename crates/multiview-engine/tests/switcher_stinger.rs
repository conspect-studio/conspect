//! ADR-0112 §5: the STINGER transition, rendered inside the one Program drive.
//!
//! Deterministic per-frame proof through the switcher frame hook: before the
//! clip, during the pre-roll, mid mix (with the clip on top), after the take
//! swapped the buses, during the clip tail and at the end, each frame equals
//! the CPU reference composite of the expected tile stack byte for byte, for
//! an opaque clip and for an alpha clip (`AlphaPlanePool::attach`). A stale
//! player is skipped and the take completes on schedule. The player roll is
//! one wait-free `fetch_or` into a fixed pending mask: a roll worker that
//! blocks, panics or never drains never touches the clock. Plus a stinger
//! command flood against the clock (#1, #10).
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
use multiview_compositor::pipeline::{self, AlphaPlanePool, CanvasColor, Nv12Image, Tile};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, Layout};
use multiview_core::switcher::{FrameCount, SourceRef, TransitionDescriptor};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::clock::{ManualTimeSource, Tick};
use multiview_engine::player_roll::{PlayerRollSender, MAX_ROLL_PLAYERS};
use multiview_engine::runtime::FrameControl;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb, COMMAND_CAPACITY,
    MAX_COMMANDS_PER_TICK,
};
use multiview_engine::switcher_hook::SwitcherFrameHook;
use multiview_engine::{
    CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, StopSignal,
    TimeSource,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const W: u32 = 64;
const TICK_NS: i64 = 40_000_000;
const A_Y: u8 = 200;
const B_Y: u8 = 60;
const CLIP_Y: u8 = 120;

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

/// How the player's clip frame is built.
#[derive(Clone, Copy)]
enum Clip {
    /// No alpha plane: opaque.
    Opaque,
    /// Left half opaque, right half transparent, and a uniform 50 % band in
    /// the bottom quarter.
    Alpha,
}

fn alpha_plane() -> Vec<u8> {
    let mut alpha = vec![0_u8; (W * W) as usize];
    for y in 0..W {
        for x in 0..W {
            alpha[(y * W + x) as usize] = if y >= W * 3 / 4 {
                128
            } else if x < W / 2 {
                255
            } else {
                0
            };
        }
    }
    alpha
}

fn clip_frame(clip: Clip, pool: &Arc<AlphaPlanePool>) -> Nv12Image {
    match clip {
        Clip::Opaque => solid(CLIP_Y),
        Clip::Alpha => pool.attach(solid(CLIP_Y), &alpha_plane()).unwrap(),
    }
}

struct Rig {
    a: Arc<TileStore<Nv12Image>>,
    b: Arc<TileStore<Nv12Image>>,
    p: Arc<TileStore<Nv12Image>>,
    pool: Arc<AlphaPlanePool>,
    handle: SwitcherControl,
    hook: SwitcherFrameHook<fn(&mut CompositorDrive<Nv12Image>)>,
    drive: CompositorDrive<Nv12Image>,
}

fn noop(_: &mut CompositorDrive<Nv12Image>) {}

/// A rig whose player store `p` goes stale 60 ms after its last frame, and
/// whose driver rolls through `roll` when one is given.
fn rig(roll: Option<PlayerRollSender>) -> Rig {
    let a = Arc::new(TileStore::<Nv12Image>::with_defaults("a"));
    let b = Arc::new(TileStore::<Nv12Image>::with_defaults("b"));
    let p = Arc::new(TileStore::<Nv12Image>::new(
        "p",
        TileThresholds::from_millis(60, 2_000, 10_000).unwrap(),
        NoSignalPolicy::Slate,
    ));
    let stores = HashMap::from([
        ("a".into(), Arc::clone(&a)),
        ("b".into(), Arc::clone(&b)),
        ("p".into(), Arc::clone(&p)),
    ]);
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
    if let Some(roll) = roll {
        driver.set_player_roll(roll);
    }
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
        p,
        pool: Arc::new(AlphaPlanePool::default()),
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

    /// One fresh output tick: both sources publish, and the player too when
    /// `clip` is given.
    fn step(&mut self, index: u64, clip: Option<Clip>) -> Arc<Nv12Image> {
        let t = tick(index);
        self.a.publish(solid(A_Y), t.pts);
        self.b.publish(solid(B_Y), t.pts);
        if let Some(clip) = clip {
            self.p.publish(clip_frame(clip, &self.pool), t.pts);
        }
        self.hook.before_compose(&mut self.drive, t);
        let frame = self.drive.compose(t).unwrap();
        self.hook.after_compose(&frame);
        Arc::clone(&frame.canvas)
    }
}

fn stinger(pre_roll: u32, clip: u32, trigger: u32, mix: u32) -> TransitionDescriptor {
    serde_json::from_value(serde_json::json!({
        "kind": "stinger", "player": "p", "pre_roll": pre_roll,
        "clip_duration": clip, "trigger_point": trigger, "mix_rate": mix,
    }))
    .unwrap()
}

/// The CPU reference: `under` at 1, `over` at `weight`, the clip on top.
fn reference(under: u8, over: Option<(u8, f32)>, clip: Option<Clip>) -> Nv12Image {
    let pool = Arc::new(AlphaPlanePool::default());
    let under = solid(under);
    let over = over.map(|(y, w)| (solid(y), w));
    let clip = clip.map(|c| clip_frame(c, &pool));
    let mut tiles = vec![Tile::scaled(&under, 0, 0, W, W, 1.0)];
    if let Some((image, weight)) = &over {
        tiles.push(Tile::scaled(image, 0, 0, W, W, *weight));
    }
    if let Some(image) = &clip {
        tiles.push(Tile::scaled(image, 0, 0, W, W, 1.0));
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

/// Select `descriptor`, AUTO on tick 2; return the frames of ticks 0..20.
/// The AUTO's duration is ignored: a stinger keeps its own timing.
fn run(rig: &mut Rig, descriptor: TransitionDescriptor, clip: Clip) -> Vec<Arc<Nv12Image>> {
    rig.send("set", SwitcherVerb::SetTransition(descriptor));
    let mut frames = vec![rig.step(0, Some(clip)), rig.step(1, Some(clip))];
    rig.send(
        "auto",
        SwitcherVerb::Auto(FrameCount::try_from(99).unwrap()),
    );
    for index in 2..20 {
        frames.push(rig.step(index, Some(clip)));
    }
    frames
}

/// Pre-roll 2, clip 10, trigger 3, mix 4, AUTO on tick 2 (elapsed `e` is
/// the tick minus 2): pre-roll on e 0..2, clip on e 2..12, mix on e 5..9 at
/// 1/4, 2/4, 3/4 and 4/4; the take swaps on e 9; the flight ends on e 12.
#[test]
fn an_alpha_stinger_matches_the_cpu_reference_at_every_phase() {
    let mut rig = rig(None);
    let mut outcomes = rig.handle.outcomes();
    let frames = run(&mut rig, stinger(2, 10, 3, 4), Clip::Alpha);
    let clip = Some(Clip::Alpha);
    assert_frame(&frames[1], &reference(A_Y, None, None), "before the AUTO");
    assert_frame(&frames[2], &reference(A_Y, None, None), "pre-roll e0");
    assert_frame(&frames[3], &reference(A_Y, None, None), "pre-roll e1");
    assert_frame(
        &frames[4],
        &reference(A_Y, None, clip),
        "clip, before trigger",
    );
    assert_frame(&frames[6], &reference(A_Y, None, clip), "clip, e4");
    assert_frame(
        &frames[7],
        &reference(A_Y, Some((B_Y, 0.25)), clip),
        "mix 1/4",
    );
    assert_frame(
        &frames[8],
        &reference(A_Y, Some((B_Y, 0.5)), clip),
        "mix 2/4",
    );
    assert_frame(
        &frames[9],
        &reference(A_Y, Some((B_Y, 0.75)), clip),
        "mix 3/4",
    );
    assert_frame(
        &frames[10],
        &reference(A_Y, Some((B_Y, 1.0)), clip),
        "mix 4/4",
    );
    assert_frame(&frames[11], &reference(B_Y, None, clip), "after the swap");
    assert_frame(&frames[13], &reference(B_Y, None, clip), "clip tail e11");
    assert_frame(&frames[14], &reference(B_Y, None, None), "the end");
    assert_frame(&frames[19], &reference(B_Y, None, None), "after the end");
    // The take's outcome is reported on its swap tick (e 9 = tick 11).
    let mut done = HashMap::new();
    while let Ok(item) = outcomes.try_recv() {
        done.insert(item.event.operation_id.clone(), item.event.clone());
    }
    let auto = &done["auto"];
    assert!(auto.completed && auto.error.is_none(), "{auto:?}");
    assert_eq!(auto.tick, 11);
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert_eq!(snapshot.preview.as_str(), "a");
    assert!(!snapshot.in_flight);
    assert_eq!(snapshot.completion_count, 1);
    assert_eq!(rig.drive.stinger_clip(), None);
}

/// An opaque clip covers the whole frame for its whole window; a one-frame
/// mix is a cut on the trigger point, under the clip.
#[test]
fn an_opaque_stinger_covers_program_and_a_one_frame_mix_cuts_under_it() {
    let mut rig = rig(None);
    let frames = run(&mut rig, stinger(1, 6, 2, 1), Clip::Opaque);
    let clip = Some(Clip::Opaque);
    assert_frame(&frames[2], &reference(A_Y, None, None), "pre-roll");
    for frame in &frames[3..9] {
        assert_frame(frame, &reference(A_Y, None, clip), "covered");
        assert_frame(frame, &reference(B_Y, None, clip), "covered");
    }
    assert_frame(&frames[9], &reference(B_Y, None, None), "the end");
    // The cut lands on the trigger point: the take swapped on e 4 (tick 6).
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    // Same timing with an alpha clip makes the cut visible beneath it.
    let mut rig = self::rig(None);
    let frames = run(&mut rig, stinger(1, 6, 2, 1), Clip::Alpha);
    let clip = Some(Clip::Alpha);
    assert_frame(&frames[4], &reference(A_Y, None, clip), "before the cut");
    assert_frame(&frames[5], &reference(A_Y, Some((B_Y, 1.0)), clip), "cut");
    assert_frame(&frames[6], &reference(B_Y, None, clip), "after the cut");
}

/// A stale player frame is never drawn (no slate, no held frame): Program is
/// the take beneath, which still completes on schedule. A fresh frame again
/// is drawn again.
#[test]
fn a_stale_player_is_skipped_and_the_take_completes_on_schedule() {
    let mut rig = rig(None);
    rig.send("set", SwitcherVerb::SetTransition(stinger(2, 10, 3, 4)));
    rig.step(0, Some(Clip::Alpha));
    rig.step(1, Some(Clip::Alpha));
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()));
    let mut frames = vec![];
    for index in 2..20 {
        // The player publishes only on ticks 2..=4, then again on 12.
        let clip = (index <= 4 || index == 12).then_some(Clip::Alpha);
        frames.push((index, rig.step(index, clip)));
    }
    let at = |i: u64| &frames.iter().find(|(t, _)| *t == i).unwrap().1;
    assert_frame(
        at(4),
        &reference(A_Y, None, Some(Clip::Alpha)),
        "fresh clip",
    );
    // 80 ms after its last frame the player is stale: the mix alone.
    assert_frame(
        at(8),
        &reference(A_Y, Some((B_Y, 0.5)), None),
        "stale mid mix",
    );
    assert_frame(at(11), &reference(B_Y, None, None), "swap without a clip");
    assert_frame(
        at(12),
        &reference(B_Y, None, Some(Clip::Alpha)),
        "fresh again",
    );
    assert_frame(
        at(13),
        &reference(B_Y, None, Some(Clip::Alpha)),
        "within hold",
    );
    assert_frame(at(14), &reference(B_Y, None, None), "end");
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "b");
    // A player that never published at all: the same take, no clip, ever.
    let mut rig = self::rig(None);
    rig.send("set", SwitcherVerb::SetTransition(stinger(2, 10, 3, 4)));
    rig.step(0, None);
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()));
    for index in 1..9 {
        rig.step(index, None);
    }
    let mid = rig.step(9, None);
    assert_frame(
        &mid,
        &reference(A_Y, Some((B_Y, 1.0)), None),
        "never published",
    );
    rig.step(10, None);
    assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "b");
}

/// The AUTO rolls the bound player exactly once, on its tick, through the
/// fixed pending mask; a roll whose player is still pending coalesces (and
/// is counted) instead of waiting or growing.
#[test]
fn the_auto_rolls_its_player_once_through_a_bounded_queue() {
    let (roll, mut rx) = PlayerRollSender::channel(["p".to_owned(), "q".to_owned()]);
    let probe = roll.clone();
    let mut rig = rig(Some(roll));
    rig.send("set", SwitcherVerb::SetTransition(stinger(2, 4, 1, 2)));
    rig.step(0, None);
    assert!(rx.try_recv().is_none(), "selecting does not roll");
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()));
    rig.step(1, None);
    assert_eq!(rx.try_recv(), Some(0), "rolled player p on the AUTO tick");
    for index in 2..8 {
        rig.step(index, None);
    }
    assert!(rx.try_recv().is_none(), "rolled exactly once");
    assert_eq!(probe.sent(), 1);
    // A plain MIX never rolls.
    rig.send(
        "mix",
        SwitcherVerb::SetTransition(TransitionDescriptor::Mix {
            rate: FrameCount::try_from(2).unwrap(),
        }),
    );
    rig.send(
        "auto2",
        SwitcherVerb::Auto(FrameCount::try_from(2).unwrap()),
    );
    for index in 8..12 {
        rig.step(index, None);
    }
    assert!(rx.try_recv().is_none());
    // Nobody drains: the first roll of q is pending, every later one
    // coalesces into it, and the caller never blocks.
    for _ in 0..7 {
        probe.roll(&SourceRef::new("q"));
    }
    assert_eq!(probe.sent(), 2);
    assert_eq!(probe.coalesced(), 6);
    assert!(!probe.roll(&SourceRef::new("not-a-player")));
    assert_eq!(probe.dropped(), 1);
    assert_eq!(
        rx.try_recv(),
        Some(1),
        "q rolls once for the coalesced burst"
    );
    assert!(rx.try_recv().is_none());
    // A player registered beyond the mask's width is dropped, never rolled.
    let ids: Vec<String> = (0..=MAX_ROLL_PLAYERS).map(|i| format!("m{i}")).collect();
    let (wide, mut wide_rx) = PlayerRollSender::channel(ids);
    assert!(wide.roll(&SourceRef::new(format!("m{}", MAX_ROLL_PLAYERS - 1))));
    assert!(!wide.roll(&SourceRef::new(format!("m{MAX_ROLL_PLAYERS}"))));
    assert_eq!(wide.dropped(), 1);
    assert_eq!(wide_rx.try_recv(), Some(MAX_ROLL_PLAYERS - 1));
}

/// SA-1 chaos: the receiver is suspended for the whole run (held, never
/// drained), while the real runtime emits 300 ticks with a stinger AUTO
/// every 12 ticks. Every tick completes, in order, with no repeat; every
/// AUTO's roll is accounted as sent or coalesced, and the pending state
/// stays one bit per player, so nothing grows and nothing waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspended_roll_receiver_never_stalls_the_tick() {
    const TICKS: u64 = 300;
    let (roll, mut rx) = PlayerRollSender::channel(["p".to_owned()]);
    let probe = roll.clone();
    let rig = rig(Some(roll));
    let (a, b, p) = (Arc::clone(&rig.a), Arc::clone(&rig.b), Arc::clone(&rig.p));
    let pool = Arc::clone(&rig.pool);
    let handle = rig.handle.clone();
    handle
        .try_send(SwitcherCommand {
            operation_id: "set".into(),
            scene_version: 1,
            verb: SwitcherVerb::SetTransition(stinger(1, 8, 2, 3)),
        })
        .unwrap();
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
    let mut indices = Vec::new();
    let mut autos = 0_u64;
    let pace = Arc::clone(&clock);
    a.publish(solid(A_Y), MediaTime::ZERO);
    b.publish(solid(B_Y), MediaTime::ZERO);
    p.publish(clip_frame(Clip::Alpha, &pool), MediaTime::ZERO);
    let started = std::time::Instant::now();
    runtime
        .run_for_with_frame_control(
            &publisher,
            &StopSignal::new(),
            TICKS,
            |frame| {
                indices.push(frame.tick.index);
                if frame.tick.index % 12 == 1 {
                    let sent = handle.try_send(SwitcherCommand {
                        operation_id: format!("auto-{}", frame.tick.index),
                        scene_version: 1,
                        verb: SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()),
                    });
                    autos += u64::from(sent.is_ok());
                }
                pace.advance(std::time::Duration::from_millis(40));
                let next =
                    MediaTime::from_nanos(i64::try_from(frame.tick.index + 1).unwrap() * TICK_NS);
                a.publish(solid(A_Y), next);
                b.publish(solid(B_Y), next);
                p.publish(clip_frame(Clip::Alpha, &pool), next);
                0
            },
            |_| None,
            rig.hook,
        )
        .await
        .unwrap();
    assert_eq!(u64::try_from(indices.len()).unwrap(), TICKS);
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous ticks"
    );
    assert_eq!(repeated.load(Ordering::Acquire), 0, "no repeated frame");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "nothing waited for the receiver"
    );
    let completions = handle.snapshot().unwrap().completion_count;
    assert!(completions >= 20, "stingers ran: {completions}");
    // Exactly one roll became pending; every later AUTO coalesced into it.
    assert_eq!(probe.sent(), 1);
    assert_eq!(probe.coalesced(), completions - 1);
    assert!(autos >= completions);
    assert_eq!(probe.dropped(), 0);
    // The receiver resumes: one pending roll, nothing queued behind it.
    assert_eq!(rx.try_recv(), Some(0));
    assert!(rx.try_recv().is_none());
}

/// A roll worker that blocks forever, or panics, never affects the tick:
/// every frame is still the exact reference and the take completes.
#[test]
fn a_blocking_or_panicking_roll_never_affects_the_tick() {
    for blocking in [true, false] {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let worker_gate = Arc::clone(&gate);
        let calls = Arc::new(AtomicU64::new(0));
        let worker_calls = Arc::clone(&calls);
        let roll = PlayerRollSender::spawn(["p".to_owned()], move |_| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            if blocking {
                let _blocked = worker_gate.lock();
            } else {
                panic!("a broken player roll");
            }
        })
        .unwrap();
        let probe = roll.clone();
        let mut rig = rig(Some(roll));
        let started = std::time::Instant::now();
        let frames = run(&mut rig, stinger(2, 10, 3, 4), Clip::Alpha);
        assert_frame(
            &frames[8],
            &reference(A_Y, Some((B_Y, 0.5)), Some(Clip::Alpha)),
            "mid mix",
        );
        assert_frame(&frames[14], &reference(B_Y, None, None), "end");
        assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "b");
        assert_eq!(probe.sent(), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "nothing waited for the roll"
        );
        // A second stinger still rolls (the panicking worker survived) or is
        // queued behind the blocked one, never waiting either way.
        rig.send(
            "auto-2",
            SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()),
        );
        for index in 20..34 {
            rig.step(index, Some(Clip::Alpha));
        }
        assert_eq!(rig.handle.snapshot().unwrap().program.as_str(), "a");
        assert_eq!(probe.sent() + probe.coalesced(), 2);
        drop(held);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while calls.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "blocking={blocking}");
    }
}

/// A CUT during the clip tail is a new take; a CUT before the swap completes
/// the stinger's take at once and ends the flight.
#[test]
fn a_cut_during_a_stinger_takes_at_once() {
    let mut rig = rig(None);
    rig.send("set", SwitcherVerb::SetTransition(stinger(2, 10, 3, 4)));
    rig.step(0, Some(Clip::Alpha));
    rig.send("auto", SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()));
    rig.step(1, Some(Clip::Alpha));
    rig.step(2, Some(Clip::Alpha));
    rig.send("cut", SwitcherVerb::Cut);
    let frame = rig.step(3, Some(Clip::Alpha));
    assert_frame(&frame, &reference(B_Y, None, None), "cut ends the stinger");
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert!(!snapshot.in_flight);
    // A stinger in its clip tail, then a CUT: a second take back to `a`.
    rig.send(
        "auto-2",
        SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()),
    );
    for index in 4..14 {
        rig.step(index, Some(Clip::Alpha));
    }
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "a", "the second stinger swapped");
    assert!(snapshot.in_flight, "in its clip tail");
    rig.send("cut-2", SwitcherVerb::Cut);
    let frame = rig.step(14, Some(Clip::Alpha));
    assert_frame(&frame, &reference(B_Y, None, None), "tail cut");
    let snapshot = rig.handle.snapshot().unwrap();
    assert_eq!(snapshot.program.as_str(), "b");
    assert!(!snapshot.in_flight);
    assert_eq!(snapshot.completion_count, 3);
}

/// Invariant #1 allocation gate: repeated stingers never grow the scratch
/// pool after the first one sized it.
#[test]
fn repeated_stingers_never_grow_the_scratch_pool() {
    let mut rig = rig(None);
    rig.send("set", SwitcherVerb::SetTransition(stinger(1, 6, 2, 2)));
    rig.step(0, Some(Clip::Alpha));
    let mut index = 1;
    let mut reservations = None;
    for round in 0..5 {
        rig.send(
            &format!("auto-{round}"),
            SwitcherVerb::Auto(FrameCount::try_from(5).unwrap()),
        );
        for _ in 0..9 {
            rig.step(index, Some(Clip::Alpha));
            index += 1;
        }
        let now = rig.drive.scratch_reservations();
        if let Some(before) = reservations {
            assert_eq!(now, before, "round {round}: the scratch pool did not grow");
        }
        reservations = Some(now);
    }
    assert_eq!(rig.handle.snapshot().unwrap().completion_count, 5);
}

/// A flood of stinger selections, AUTOs and CUTs (valid and invalid) from
/// another thread while the real runtime emits 600 ticks, with a roll worker
/// that blocks forever: exactly one frame per tick, no repeat, the producer
/// meets a full mailbox, at most 16 commands a tick, rolls bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stinger_command_flood_never_moves_the_output_clock() {
    const TICKS: u64 = 600;
    // The worker blocks on a channel nobody sends on until the end.
    let (release, wedge) = std::sync::mpsc::channel::<()>();
    let wedge = Mutex::new(wedge);
    let roll = PlayerRollSender::spawn(["p".to_owned()], move |_| {
        if let Ok(wedge) = wedge.lock() {
            let _ = wedge.recv();
        }
    })
    .unwrap();
    let probe = roll.clone();
    let rig = rig(Some(roll));
    let (a, b, p) = (Arc::clone(&rig.a), Arc::clone(&rig.b), Arc::clone(&rig.p));
    let pool = Arc::clone(&rig.pool);
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
    for (op, verb) in [
        (
            "seed-set",
            SwitcherVerb::SetTransition(stinger(2, 20, 5, 6)),
        ),
        (
            "seed-auto",
            SwitcherVerb::Auto(FrameCount::try_from(6).unwrap()),
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
    let producer = {
        let handle = handle.clone();
        let (done, accepted, full) = (Arc::clone(&done), Arc::clone(&accepted), Arc::clone(&full));
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            while !done.load(Ordering::Acquire) {
                let op = format!("op-{n}");
                let verb = match n % 6 {
                    0 => SwitcherVerb::SetTransition(stinger(
                        u32::try_from(n % 4).unwrap() + 1,
                        12,
                        3,
                        u32::try_from(n % 3).unwrap() + 1,
                    )),
                    // Invalid: the mix window overruns the clip.
                    1 => SwitcherVerb::SetTransition(stinger(1, 4, 3, 4)),
                    4 if n % 60 == 4 => SwitcherVerb::Cut,
                    4 => SwitcherVerb::SetTransition(TransitionDescriptor::Mix {
                        rate: FrameCount::try_from(4).unwrap(),
                    }),
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
    p.publish(clip_frame(Clip::Alpha, &pool), MediaTime::ZERO);
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
                p.publish(clip_frame(Clip::Alpha, &pool), next);
                0
            },
            |_| None,
            rig.hook,
        )
        .await
        .unwrap();
    done.store(true, Ordering::Release);
    producer.join().unwrap();
    drop(release);
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
    // Stingers ran and rolled; the wedged worker made later rolls coalesce
    // into the one pending bit rather than wait or grow.
    assert!(probe.sent() >= 1, "at least one stinger rolled");
    assert!(
        probe.sent() <= 2,
        "a wedged worker holds at most one roll plus one pending: {}",
        probe.sent()
    );
    let selection = handle.snapshot().unwrap().transition.clone();
    assert!(
        selection.is_rendered() && selection.is_valid(),
        "{selection:?}"
    );
}
