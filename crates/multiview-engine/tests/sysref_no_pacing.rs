//! Invariant #1 re-assertion for the system-clock discipline reference: the
//! NTP/PTP lock auto-detect **informs the wall-clock badge only** — it never
//! paces, gates, or stalls the output clock.
//!
//! This proves the structural property the way the PTP `ptp_no_pacing.rs` test
//! does for the servo: the [`OutputClock`] is advanced purely from its integer
//! tick counter (`out_pts = f(tick)`) and the produced PTS stream is byte
//! identical whether or not a `SystemRefTracker` + `ReferenceSelector` are
//! concurrently churning through lock states. The selector exposes no method the
//! output clock calls and holds no reference to it; a tracker whose injected
//! `adjtimex` read fails on every sample changes only the badge, never the tick.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{MediaTime, Rational};
use multiview_engine::ptp::{LockState, ReferenceStatus};
use multiview_engine::sysref::{
    NtpQuery, NtpReading, ReferenceSelector, SystemRefConfig, SystemRefTracker,
};
use multiview_engine::OutputClock;

/// An NTP source whose `adjtimex` read always fails (models a container without
/// the capability): every sample falls back to the assumed state.
struct AlwaysUnavailable;

impl NtpQuery for AlwaysUnavailable {
    fn read(&mut self) -> Option<NtpReading> {
        None
    }
}

fn ptp_freerun() -> ReferenceStatus {
    ReferenceStatus {
        state: LockState::Freerun,
        offset_ns: 0,
        frequency_ppb: 0,
        accepted: 0,
        disciplined: false,
    }
}

/// Run an output clock for `n` ticks, optionally sampling the discipline
/// reference (which falls back through Freerun) on every tick, and collect the
/// PTS of every tick.
fn run(n: u64, sample_reference: bool) -> Vec<MediaTime> {
    let mut clock = OutputClock::new(Rational::FPS_59_94).expect("valid cadence");
    let mut tracker = SystemRefTracker::new(SystemRefConfig {
        est_error_tolerance_ns: 100_000,
        assumed_when_unavailable: LockState::Freerun,
    });
    let mut ntp = AlwaysUnavailable;
    let selector = ReferenceSelector::default();
    let mut out = Vec::new();
    for _ in 0..n {
        if sample_reference {
            // Sample the badge *between* ticks. Whatever it reports must not
            // influence how many frames the clock emits or when.
            let sys_state = tracker.sample(&mut ntp);
            let chosen = selector.select(sys_state, tracker.offset_ns(), &ptp_freerun());
            // Touch the result so the optimiser cannot elide the sampling.
            assert!(!chosen.state.is_disciplined() || chosen.state.is_disciplined());
        }
        out.push(clock.tick().pts);
    }
    out
}

#[test]
fn discipline_reference_never_changes_the_tick_stream() {
    const TICKS: u64 = 100_000;
    let baseline = run(TICKS, false);
    let with_reference = run(TICKS, true);
    assert_eq!(
        baseline.len(),
        with_reference.len(),
        "the discipline reference must not change how many frames the clock emits"
    );
    assert_eq!(
        baseline, with_reference,
        "out_pts = f(tick) must be byte-identical with the discipline reference churning vs. off"
    );
}
