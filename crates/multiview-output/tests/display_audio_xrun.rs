//! ALSA xrun-recovery state-machine tests (DEV-B4 / bad-inputs-are-the-purpose).
//!
//! An ALSA underrun (`-EPIPE`) or suspend (`-ESTRPIPE`) must NEVER crash the
//! sink: the recovery machine prepares/resumes the PCM and re-primes, holding
//! audio rather than faltering — the display/audio never goes black. These
//! tests drive the pure state machine over scripted ALSA outcomes (no
//! hardware): a recoverable error transitions to Recovering→Running, a
//! repeated/unrecoverable error backs off and stays Degraded (silent) without
//! ever panicking, and a clean write keeps it Running.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::display::audio::{PcmOutcome, XrunRecovery, XrunState};

#[test]
fn clean_writes_stay_running() {
    let mut sm = XrunRecovery::new();
    assert_eq!(sm.state(), XrunState::Priming);
    sm.on_outcome(PcmOutcome::Wrote(480));
    assert_eq!(sm.state(), XrunState::Running);
    sm.on_outcome(PcmOutcome::Wrote(480));
    assert_eq!(sm.state(), XrunState::Running);
}

#[test]
fn underrun_triggers_recovery_then_resumes() {
    let mut sm = XrunRecovery::new();
    sm.on_outcome(PcmOutcome::Wrote(480));
    // An -EPIPE underrun: the machine must request a recover action, not crash.
    let action = sm.on_outcome(PcmOutcome::Underrun);
    assert_eq!(sm.state(), XrunState::Recovering);
    assert!(
        action.recover,
        "underrun must request a PCM prepare/recover"
    );
    // After a successful recover + first good write, back to Running.
    sm.on_outcome(PcmOutcome::Recovered);
    sm.on_outcome(PcmOutcome::Wrote(480));
    assert_eq!(sm.state(), XrunState::Running);
}

#[test]
fn suspend_is_recoverable_via_resume() {
    let mut sm = XrunRecovery::new();
    sm.on_outcome(PcmOutcome::Wrote(480));
    let action = sm.on_outcome(PcmOutcome::Suspended);
    assert_eq!(sm.state(), XrunState::Recovering);
    assert!(action.recover, "suspend must request a resume/prepare");
    sm.on_outcome(PcmOutcome::Recovered);
    sm.on_outcome(PcmOutcome::Wrote(480));
    assert_eq!(sm.state(), XrunState::Running);
}

#[test]
fn repeated_failure_backs_off_and_stays_silent_not_crash() {
    // If recovery keeps failing, the machine must NOT spin or crash: it enters a
    // Degraded (silent) state, backs off, and keeps trying — the sink stays
    // alive and the rest of the display path is untouched.
    let mut sm = XrunRecovery::new();
    sm.on_outcome(PcmOutcome::Wrote(480));
    for _ in 0..10 {
        sm.on_outcome(PcmOutcome::Underrun);
        sm.on_outcome(PcmOutcome::RecoverFailed);
    }
    assert_eq!(sm.state(), XrunState::Degraded);
    assert!(
        sm.backoff().as_millis() > 0,
        "a degraded sink backs off before re-trying"
    );
    // A later successful recover lifts it back out of Degraded.
    sm.on_outcome(PcmOutcome::Recovered);
    sm.on_outcome(PcmOutcome::Wrote(480));
    assert_eq!(sm.state(), XrunState::Running);
}

#[test]
fn recovery_count_is_observable_telemetry() {
    let mut sm = XrunRecovery::new();
    sm.on_outcome(PcmOutcome::Wrote(480));
    assert_eq!(sm.recoveries(), 0);
    sm.on_outcome(PcmOutcome::Underrun);
    sm.on_outcome(PcmOutcome::Recovered);
    assert_eq!(sm.recoveries(), 1, "each recovery is counted for telemetry");
}
