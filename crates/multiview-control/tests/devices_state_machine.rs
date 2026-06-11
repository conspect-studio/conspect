//! The device runtime state machine (ADR-M008 §2.2): the typed transition table
//! `DISCOVERED → ADOPTING → ONLINE / DEGRADED / AUTH_FAILED / UNREACHABLE`. This
//! is the model the future driver actors (DEV-A4/A5) drive; here it is exercised
//! exhaustively with no real device I/O.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::devices::{DeviceLifecycle, LifecycleEvent};
use multiview_events::DeviceState;

#[test]
fn adopting_to_online_on_probe_ok() {
    let mut lc = DeviceLifecycle::new();
    assert_eq!(lc.state(), DeviceState::Adopting);
    let changed = lc.apply(LifecycleEvent::ProbeOk);
    assert!(changed, "ADOPTING + ProbeOk transitions");
    assert_eq!(lc.state(), DeviceState::Online);
}

#[test]
fn adopting_to_auth_failed_on_bad_credentials() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::AuthRejected);
    assert_eq!(lc.state(), DeviceState::AuthFailed);
}

#[test]
fn adopting_to_unreachable_on_probe_timeout() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::Unreachable);
    assert_eq!(lc.state(), DeviceState::Unreachable);
}

#[test]
fn online_to_degraded_on_device_fault_and_back_on_recover() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::ProbeOk);
    assert_eq!(lc.state(), DeviceState::Online);
    lc.apply(LifecycleEvent::DeviceFault);
    assert_eq!(lc.state(), DeviceState::Degraded);
    lc.apply(LifecycleEvent::Recover);
    assert_eq!(
        lc.state(),
        DeviceState::Online,
        "DEGRADED recovers to ONLINE"
    );
}

#[test]
fn online_to_unreachable_and_reconnect_back_to_online() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::ProbeOk);
    lc.apply(LifecycleEvent::Unreachable);
    assert_eq!(lc.state(), DeviceState::Unreachable);
    lc.apply(LifecycleEvent::Reconnect);
    assert_eq!(
        lc.state(),
        DeviceState::Online,
        "UNREACHABLE reconnects back to ONLINE (re-converge)"
    );
}

#[test]
fn auth_failed_only_clears_on_secret_update() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::AuthRejected);
    assert_eq!(lc.state(), DeviceState::AuthFailed);
    // The breaker is open: a bare reconnect attempt does not leave AUTH_FAILED.
    let changed = lc.apply(LifecycleEvent::Reconnect);
    assert!(!changed, "AUTH_FAILED ignores a reconnect (breaker open)");
    assert_eq!(lc.state(), DeviceState::AuthFailed);
    // Only a secret update re-arms a probe.
    lc.apply(LifecycleEvent::SecretUpdated);
    assert_eq!(lc.state(), DeviceState::Adopting);
}

#[test]
fn degraded_to_unreachable_when_the_management_channel_drops() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::ProbeOk);
    lc.apply(LifecycleEvent::DeviceFault);
    assert_eq!(lc.state(), DeviceState::Degraded);
    lc.apply(LifecycleEvent::Unreachable);
    assert_eq!(lc.state(), DeviceState::Unreachable);
}

#[test]
fn apply_reports_no_change_for_an_inapplicable_event() {
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::ProbeOk);
    // ONLINE + ProbeOk is a no-op (already online).
    let changed = lc.apply(LifecycleEvent::ProbeOk);
    assert!(!changed, "an idempotent event reports no transition");
    assert_eq!(lc.state(), DeviceState::Online);
}

#[test]
fn online_auth_rejection_opens_the_breaker() {
    // A live device whose credentials are rotated out from under it: a probe
    // that now authfails moves ONLINE → AUTH_FAILED, not UNREACHABLE.
    let mut lc = DeviceLifecycle::new();
    lc.apply(LifecycleEvent::ProbeOk);
    lc.apply(LifecycleEvent::AuthRejected);
    assert_eq!(lc.state(), DeviceState::AuthFailed);
}
