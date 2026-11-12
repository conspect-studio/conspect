//! Issue #321 RED (first commit): a whole-process stall, injected with
//! SIGSTOP/SIGCONT from a child shell, makes the slice-1 chaos assertion
//! (`repeats == 0`) fail although the clock did exactly what ADR-T018 says.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::print_stderr)]

mod common;

use std::time::Duration;

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

#[test]
fn a_whole_process_stall_fails_the_slice_1_assertion() {
    let stopper = sigstop_self_after(Duration::from_secs(1), Duration::from_millis(150));
    let run = common::run_clock(Duration::from_secs(3), |_| {});
    stopper.join().unwrap();
    common::report("sigstop", &run);
    common::old_assert_cadence("sigstop", &run);
}
