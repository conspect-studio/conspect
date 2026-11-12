//! Chaos gate (invariants #1 and #10, ADR-0108 §2.2/§4.4): the output clock's
//! one new duty, a `try_send` of each fresh tick to the `routing-aux` thread,
//! costs the clock nothing when the aux thread is wedged or dead.
//!
//! Real `EngineRuntime` + `RealtimePacer` over the monotonic clock at
//! 30000/1001. Bounds asserted per scenario, against a baseline run with the
//! sender inactive:
//! - every scenario emits exactly `TICKS` ticks;
//! - `frames_repeated` is no larger than the baseline run's;
//! - wedged aux (never reads): `dropped == fresh - 4` (the channel holds 4);
//! - panicking aux (reads one tick, then panics): `fresh - 5 <= dropped <= fresh - 1`
//!   (up to 4 ticks may land in the buffer before the receiver is dropped);
//! - inactive sender: zero sends and zero drops.
//!
//! Each run is 90 ticks (about 3 s); the four runs are sequential so they do
//! not contend with each other (about 12 s total).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::MonotonicTimeSource;
use multiview_engine::{
    aux_tick_channel, AuxTick, AuxTickSender, ClosureControl, CompositorDrive, EnginePublisher,
    EngineRuntime, OutputClock, RealtimePacer, StopSignal, TimeSource, WithAuxTick,
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
    let outcome = rt.block_on(async move {
        runtime
            .run_for_with_frame_control(
                run_pub.as_ref(),
                &stop,
                TICKS,
                |f| f.tick.index,
                |f| Some(f.tick.index),
                WithAuxTick {
                    inner: ClosureControl(|_drive: &mut CompositorDrive<Nv12Image>| {}),
                    aux,
                },
            )
            .await
            .unwrap()
    });
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
    // Baseline: the sender is inactive (no routed feed): zero sends.
    let (tx, rx) = aux_tick_channel();
    let dropped = tx.dropped_counter();
    let base = run_clock(tx);
    assert_eq!(base.ticks, TICKS);
    assert!(base.contiguous, "baseline published every tick in order");
    assert!(rx.try_recv().is_err(), "inactive sender sent a tick");
    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    // (a) Wedged aux: active, the receiver is held but never read.
    let (tx, rx_wedged) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let dropped = tx.dropped_counter();
    let wedged = run_clock(tx);
    assert_eq!(wedged.ticks, TICKS);
    assert!(wedged.contiguous);
    assert!(
        wedged.repeated <= base.repeated,
        "wedged aux repeated {} > baseline {}",
        wedged.repeated,
        base.repeated
    );
    let fresh = TICKS - wedged.repeated;
    assert_eq!(dropped.load(Ordering::Relaxed), fresh - 4);
    drop(rx_wedged);

    // (b) Panicking aux: reads the first tick then panics (receiver dropped).
    let (tx, rx) = aux_tick_channel();
    tx.active_flag().store(true, Ordering::Release);
    let dropped = tx.dropped_counter();
    let dying = spawn_panicking_receiver(rx);
    let dead = run_clock(tx);
    assert!(dying.join().is_err(), "the aux thread panicked as injected");
    assert_eq!(dead.ticks, TICKS);
    assert!(dead.contiguous);
    assert!(dead.repeated <= base.repeated);
    let fresh = TICKS - dead.repeated;
    let d = dropped.load(Ordering::Relaxed);
    assert!(
        (fresh - 5..fresh).contains(&d),
        "dropped {d} outside [{}, {})",
        fresh - 5,
        fresh
    );

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
