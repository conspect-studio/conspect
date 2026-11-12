//! Chaos gate (invariants #1 and #10, ADR-0108 §2.2/§4.4): the output clock's
//! one new duty, a `try_send` of each fresh tick to the `routing-aux` thread,
//! costs the clock nothing when the aux thread is wedged or dead.
//!
//! Two parts, each asserting only what the routing code controls (never a
//! comparison between two separate real-time runs, whose repeat counts are
//! host noise on a loaded runner):
//!
//! 1. Real `EngineRuntime` + `RealtimePacer` over the monotonic clock at
//!    30000/1001, 90 ticks per scenario (about 3 s each, sequential):
//!    - exactly `TICKS` ticks, published contiguously in order;
//!    - wedged aux (never reads): `sent == 4` (the channel holds 4) and
//!      `dropped == fresh - 4`;
//!    - panicking aux (reads one tick, then panics): `sent + dropped == fresh`,
//!      `fresh - 5 <= dropped <= fresh - 1`;
//!    - inactive sender: zero sends and zero drops;
//!    - the clock-side cost of the aux duty in the real run, measured per fresh
//!      tick around `WithAuxTick::after_compose`: max below half a tick
//!      (16.7 ms at 30000/1001, the design's Program protection bound). A real
//!      run has only ~90 samples, so its p99 is its max and measures runner
//!      preemption, not the code; the code's own cost is part 3.
//! 3. Tight loop: 10 000 `AuxTickSender::offer` calls each against a wedged
//!    (full) channel and a disconnected one, timed individually: p99 < 100 µs
//!    and median < 10 µs. This is the per-tick clock-side work routing adds.
//! 2. Deterministic `ManualTimeSource` + `CooperativePacer` (no real sleeps; the
//!    control advances the clock exactly one period per compose, as
//!    `cadence_hold.rs` does), 300 ticks: a wedged and a panicked receiver each
//!    give exactly 0 `frames_repeated`, every tick fresh and accounted for.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::{ManualTimeSource, MonotonicTimeSource};
use multiview_engine::runtime::FrameControl;
use multiview_engine::{
    aux_tick_channel, AuxTick, AuxTickSender, ClosureControl, CompositedFrame, CompositorDrive,
    CooperativePacer, EnginePublisher, EngineRuntime, OutputClock, RealtimePacer, StopSignal, Tick,
    TimeSource, WithAuxTick,
};

const TICKS: u64 = 90;

fn drive(cadence: Rational) -> CompositorDrive<Nv12Image> {
    let color = ColorInfo::default().resolve_defaults(1920, 1080);
    let layout = Layout {
        name: "aux".to_owned(),
        canvas: Canvas {
            width: 16,
            height: 16,
            fps_num: cadence.num,
            fps_den: cadence.den,
        },
        cells: Vec::new(),
    };
    CompositorDrive::new(
        Arc::new(layout),
        HashMap::new(),
        Nv12Image::solid(16, 16, 16, 128, 128, color).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

struct RunResult {
    ticks: u64,
    repeated: u64,
    contiguous: bool,
    /// Wall time of each fresh tick's `after_compose` (the aux duty), sorted.
    offer_ns: Vec<u64>,
}

/// Times the wrapped control's `after_compose` per fresh tick.
struct Timed<C> {
    inner: C,
    ns: Arc<Mutex<Vec<u64>>>,
}

impl<C: FrameControl> FrameControl for Timed<C> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.before_compose(drive, tick);
    }
    fn after_compose(&mut self, frame: &CompositedFrame) {
        let started = Instant::now();
        self.inner.after_compose(frame);
        let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        // Test bookkeeping only, outside the measured span.
        self.ns.lock().unwrap().push(ns);
    }
    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.on_repeat(drive, tick);
    }
}

/// Half a tick at 30000/1001, in nanoseconds (the design's bound for any
/// clock-side cost, output-routing.md §10): 1001/30000 s / 2 =
/// 1_001_000_000_000 ns / 30_000 / 2 = 16_683_333 ns (~16.68 ms).
const HALF_TICK_NS: u64 = 1_001_000_000_000 / 30_000 / 2;

/// The aux duty's per-tick cost in a real run: max below half a tick.
fn assert_offer_cost(name: &str, offer_ns: &[u64]) {
    assert!(!offer_ns.is_empty(), "{name}: no fresh tick measured");
    let max = *offer_ns.last().unwrap();
    assert!(
        max < HALF_TICK_NS,
        "{name}: aux duty max {max} ns >= half a tick ({HALF_TICK_NS} ns)"
    );
}

/// Run the real clock for `TICKS` ticks with the aux sender wrapped around a
/// legacy no-op control, recording every published tick index.
fn run_clock(aux: AuxTickSender) -> RunResult {
    let cadence = Rational::FPS_29_97;
    let time: Arc<dyn TimeSource> = Arc::new(MonotonicTimeSource::new());
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        time,
        RealtimePacer,
    );
    let repeated = runtime.frames_repeated_counter();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(256));
    let mut sub = publisher.subscribe();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    let stop = StopSignal::new();
    let run_pub = Arc::clone(&publisher);
    let timings = Arc::new(Mutex::new(Vec::with_capacity(
        usize::try_from(TICKS).unwrap(),
    )));
    let control = Timed {
        inner: WithAuxTick {
            inner: ClosureControl(|_drive: &mut CompositorDrive<Nv12Image>| {}),
            aux,
        },
        ns: Arc::clone(&timings),
    };
    let outcome = rt.block_on(async move {
        runtime
            .run_for_with_frame_control(
                run_pub.as_ref(),
                &stop,
                TICKS,
                |f| f.tick.index,
                |f| Some(f.tick.index),
                control,
            )
            .await
            .unwrap()
    });
    let mut offer_ns = std::mem::take(&mut *timings.lock().unwrap());
    offer_ns.sort_unstable();
    let mut seen = Vec::new();
    while let Ok(i) = sub.try_recv() {
        seen.push(*i.event);
    }
    let contiguous =
        seen.len() == usize::try_from(TICKS).unwrap() && seen.windows(2).all(|w| w[1] == w[0] + 1);
    RunResult {
        ticks: outcome.ticks,
        repeated: repeated.load(Ordering::Acquire),
        contiguous,
        offer_ns,
    }
}

fn spawn_panicking_receiver(rx: Receiver<AuxTick>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _first = rx.recv();
        panic!("aux thread dies after its first tick");
    })
}

#[test]
fn a_wedged_or_dead_aux_thread_costs_the_output_clock_nothing() {
    // Inactive sender (no routed feed): zero sends.
    let (tx, rx) = aux_tick_channel();
    let dropped = tx.dropped_counter();
    let sent = tx.sent_counter();
    let base = run_clock(tx);
    assert_eq!(base.ticks, TICKS);
    assert!(base.contiguous, "baseline published every tick in order");
    assert!(rx.try_recv().is_err(), "inactive sender sent a tick");
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    assert_eq!(sent.load(Ordering::Relaxed), 0);
    assert_offer_cost("inactive", &base.offer_ns);

    // (a) Wedged aux: active, the receiver is held but never read.
    let (tx, rx_wedged) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let dropped = tx.dropped_counter();
    let sent = tx.sent_counter();
    let wedged = run_clock(tx);
    assert_eq!(wedged.ticks, TICKS);
    assert!(wedged.contiguous);
    let fresh = TICKS - wedged.repeated;
    assert_eq!(u64::try_from(wedged.offer_ns.len()).unwrap(), fresh);
    assert_eq!(sent.load(Ordering::Relaxed), 4);
    assert_eq!(dropped.load(Ordering::Relaxed), fresh - 4);
    assert_offer_cost("wedged", &wedged.offer_ns);
    drop(rx_wedged);

    // (b) Panicking aux: reads the first tick then panics (receiver dropped).
    let (tx, rx) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let dropped = tx.dropped_counter();
    let sent = tx.sent_counter();
    let dying = spawn_panicking_receiver(rx);
    let dead = run_clock(tx);
    assert!(dying.join().is_err(), "the aux thread panicked as injected");
    assert_eq!(dead.ticks, TICKS);
    assert!(dead.contiguous);
    let fresh = TICKS - dead.repeated;
    let d = dropped.load(Ordering::Relaxed);
    assert_eq!(sent.load(Ordering::Relaxed) + d, fresh);
    assert!(
        (fresh - 5..fresh).contains(&d),
        "dropped {d} outside [{}, {})",
        fresh - 5,
        fresh
    );
    assert_offer_cost("dead", &dead.offer_ns);

    // (c) Inactive again after activity: zero sends.
    let (tx, rx) = aux_tick_channel();
    let flag = tx.active_flag();
    flag.store(true, Ordering::Release);
    flag.store(false, Ordering::Release);
    let dropped = tx.dropped_counter();
    let idle = run_clock(tx);
    assert_eq!(idle.ticks, TICKS);
    assert!(rx.try_recv().is_err());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

/// Deterministic part: 300 ticks on a manual clock that advances exactly one
/// period per compose. Returns `(ticks, frames_repeated, sent, dropped)`.
async fn run_manual(aux: AuxTickSender) -> (u64, u64, u64, u64) {
    const MANUAL_TICKS: u64 = 300;
    // An exact-period cadence (25 fps = 40 ms) so whole-period advances land
    // on the clock's deadlines (see cadence_hold.rs).
    let cadence = Rational::new(25, 1);
    let period = Duration::from_millis(40);
    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        drive(cadence),
        ts,
        CooperativePacer,
    );
    let sent = aux.sent_counter();
    let dropped = aux.dropped_counter();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let stop = StopSignal::new();
    let ts_ctl = Arc::clone(&time_source);
    let outcome = runtime
        .run_for_with_frame_control(
            publisher.as_ref(),
            &stop,
            MANUAL_TICKS,
            |f| f.tick.index,
            |_f| None::<u64>,
            WithAuxTick {
                inner: ClosureControl(move |_d: &mut CompositorDrive<Nv12Image>| {
                    ts_ctl.advance(period);
                }),
                aux,
            },
        )
        .await
        .unwrap();
    (
        outcome.ticks,
        runtime.frames_repeated(),
        sent.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_or_panicked_aux_gives_zero_repeats_on_a_manual_clock() {
    let (tx, rx_wedged) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let (ticks, repeated, sent, dropped) = run_manual(tx).await;
    assert_eq!((ticks, repeated), (300, 0), "wedged aux: zero repeats");
    assert_eq!((sent, dropped), (4, 296));
    drop(rx_wedged);

    let (tx, rx) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let dying = spawn_panicking_receiver(rx);
    let (ticks, repeated, sent, dropped) = run_manual(tx).await;
    assert!(dying.join().is_err(), "the aux thread panicked as injected");
    assert_eq!((ticks, repeated), (300, 0), "panicked aux: zero repeats");
    assert_eq!(sent + dropped, 300, "every fresh tick offered exactly once");
}

/// Part 3: the clock-side work routing adds, measured over many offers so the
/// percentiles describe the code rather than one preempted tick.
#[test]
fn an_offer_to_a_wedged_or_dead_aux_thread_is_microseconds() {
    const OFFERS: usize = 10_000;
    fn time_offers(tx: &AuxTickSender) -> Vec<u64> {
        let mut ns = Vec::with_capacity(OFFERS);
        for index in 0..OFFERS {
            let tick = AuxTick {
                index: u64::try_from(index).unwrap(),
                pts: multiview_core::time::MediaTime::ZERO,
            };
            let started = Instant::now();
            tx.offer(tick);
            ns.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        ns.sort_unstable();
        ns
    }
    let check = |name: &str, ns: &[u64]| {
        let p50 = ns[ns.len() / 2];
        // Nearest-rank p99: the smallest sample >= 99 % of all samples.
        let p99 = ns[(ns.len() * 99).div_ceil(100) - 1];
        assert!(p50 < 10_000, "{name}: offer median {p50} ns >= 10 us");
        assert!(p99 < 100_000, "{name}: offer p99 {p99} ns >= 100 us");
    };
    // Wedged: the receiver is held and never read (every offer after 4 drops).
    let (tx, rx) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let wedged = time_offers(&tx);
    assert_eq!(tx.sent_counter().load(Ordering::Relaxed), 4);
    check("wedged", &wedged);
    drop(rx);
    // Dead: the receiver is gone (the aux thread exited).
    let (tx, rx) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    drop(rx);
    let dead = time_offers(&tx);
    assert_eq!(
        tx.dropped_counter().load(Ordering::Relaxed),
        u64::try_from(OFFERS).unwrap()
    );
    check("dead", &dead);
}
