//! Issue #321: the chaos suite's clock assertion, judged on its own.
//!
//! * RED (kept as a `should_panic` record): a whole-process stall injected
//!   with SIGSTOP/SIGCONT from a child shell makes the slice-1 assertion
//!   (`repeats == 0`) fail, although the clock did exactly what ADR-T018 says
//!   (it held cadence with last-good repeats). Measured before the fix: 3/3
//!   runs failed with 3-4 repeats after a ~152 ms stall.
//! * GREEN: the same stall, judged by the stall witness: every repeat
//!   overlaps a witness gap, so `assert_cadence` passes.
//! * NEGATIVE CONTROL: the program runtime alone is blocked (a 150 ms
//!   busy-spin inside one tick's projection) while the witness keeps
//!   running: those repeats are NOT explained, so the witness never excuses a
//!   starved clock.
//! * BLOCK THEN STALL (review PR 333): a 100 ms program-runtime block
//!   immediately followed by a 150 ms SIGSTOP; the ticks the block made late
//!   are not covered by the stall's gap and stay unexplained.
//! * BLOCK UNDER FLOOD (review PR 333): a 100 ms program-runtime block while a
//!   loopback flood keeps every other thread busy; the witness's own
//!   scheduling noise must not excuse it.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::print_stderr)]

#[path = "common/witness.rs"]
mod common;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use multiview_frontdoor::flood::{Flood, FloodConfig, Shape, Sources};

/// SIGSTOP stops every thread of this test process: the tests in this file
/// run one at a time, or one test's stall would land in another's run.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sigstop_self_after(delay: Duration, stall: Duration) -> std::thread::JoinHandle<()> {
    let pid = std::process::id();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let script = format!(
            "kill -STOP {pid}; sleep {}; kill -CONT {pid}",
            stall.as_secs_f64()
        );
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .status();
    })
}

fn stalled_run() -> common::ClockRun {
    let stopper = sigstop_self_after(Duration::from_secs(1), Duration::from_millis(150));
    let run = common::run_clock(Duration::from_secs(3), |_| {});
    stopper.join().unwrap();
    run
}

#[test]
#[should_panic(expected = "no tick fell behind compose")]
fn red_a_whole_process_stall_fails_the_slice_1_assertion() {
    let _serial = serial();
    let run = stalled_run();
    common::report("sigstop (old assertion)", &run);
    common::old_assert_cadence("sigstop", &run);
}

#[test]
fn a_whole_process_stall_is_explained_by_the_witness() {
    let _serial = serial();
    let run = stalled_run();
    common::report("sigstop (witness)", &run);
    let c = common::classify(&run);
    // The stall is > 3 periods: the clock must have needed help, and the
    // witness must have seen the process stop.
    assert!(c.needing_explanation >= 3, "{c:?}");
    assert!(
        run.gaps.iter().any(|(s, e)| e - s >= 100_000_000),
        "{:?}",
        run.gaps
    );
    common::assert_cadence("sigstop", &run);
}

#[test]
fn a_starved_program_runtime_is_never_explained() {
    let _serial = serial();
    // Spin 150 ms inside tick 30's projection (on the program runtime): the
    // process keeps running, so the witness sees no gap.
    let run = common::run_clock(Duration::from_secs(3), |index| {
        if index == 30 {
            let until = Instant::now() + Duration::from_millis(150);
            while Instant::now() < until {
                std::hint::spin_loop();
            }
        }
    });
    common::report("starved program runtime", &run);
    let c = common::classify(&run);
    assert!(run.repeats >= 3, "the spin forced repeats: {}", run.repeats);
    assert!(
        !c.unexplained.is_empty(),
        "the witness must not excuse a starved clock: {c:?} gaps {:?}",
        run.gaps
    );
    let r = std::panic::catch_unwind(|| common::assert_cadence("starved", &run));
    assert!(r.is_err(), "assert_cadence fails a starved clock");
}

fn spin(d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

/// Start the stall `after` from now, from a helper thread.
fn sigstop_at(after: Duration, stall: Duration) -> std::thread::JoinHandle<()> {
    sigstop_self_after(after, stall)
}

#[test]
fn a_block_followed_by_a_stall_is_not_explained_by_the_stall() {
    let _serial = serial();
    // Tick 30 (deadline ~1 s): the projection spins 100 ms, and the stall is
    // scheduled to begin as the spin ends. The block's late ticks lie before
    // the gap starts, so the gap cannot cover them.
    let stopper = std::sync::Arc::new(Mutex::new(None));
    let slot = std::sync::Arc::clone(&stopper);
    let run = common::run_clock(Duration::from_secs(3), move |index| {
        let mut slot = slot.lock().unwrap();
        if index == 30 && slot.is_none() {
            *slot = Some(sigstop_at(
                Duration::from_millis(100),
                Duration::from_millis(150),
            ));
            drop(slot);
            spin(Duration::from_millis(100));
        }
    });
    if let Some(handle) = stopper.lock().unwrap().take() {
        handle.join().unwrap();
    }
    common::report("block then stall", &run);
    let c = common::classify(&run);
    assert!(
        !c.unexplained.is_empty(),
        "a program-runtime block before a stall must stay unexplained: {c:?} gaps {:?}",
        run.gaps
    );
}

#[test]
fn a_block_under_flood_is_not_explained_by_witness_noise() {
    let _serial = serial();
    let target = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let flood = Flood::start(FloodConfig {
        target: target.local_addr().unwrap(),
        pps: 20_000,
        threads: 4,
        sources: Sources::One("[::1]:0".parse().unwrap()),
        shapes: Shape::ALL.to_vec(),
    })
    .unwrap();
    let run = common::run_clock(Duration::from_secs(3), |index| {
        if index == 30 {
            spin(Duration::from_millis(100));
        }
    });
    let _ = flood.stop();
    drop(target);
    common::report("block under flood", &run);
    let c = common::classify(&run);
    assert!(
        run.repeats >= 2,
        "the block forced repeats: {}",
        run.repeats
    );
    assert!(
        !c.unexplained.is_empty(),
        "witness noise under flood must not excuse a blocked clock: {c:?} gaps {:?}",
        run.gaps
    );
}
