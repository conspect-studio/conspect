//! Tests for the **system-clock discipline reference** (`sysref`) — the pure
//! source-selection + lock-classification logic that feeds the wall-clock
//! reference badge.
//!
//! Two pure machines are exercised here, both over **injected** inputs (no
//! syscalls, no clocks of their own — CI has neither a synced NTP host nor a PTP
//! grandmaster):
//!
//! * [`classify_system`] / [`SystemRefTracker`] — map a kernel `adjtimex`
//!   snapshot ([`NtpReading`]) to a [`LockState`]: `STA_UNSYNC` set ⇒ `Freerun`;
//!   synchronised + estimated error within tolerance ⇒ `Locked`; synchronised but
//!   error over tolerance (or a `TIME_ERROR` clock state) ⇒ `Holdover`. The
//!   unavailable arm (no reading) falls back to a configured assumed state.
//! * [`ReferenceSelector`] — arbitrate the system (NTP/`SYS`) discipline against
//!   the PTP [`ReferenceStatus`] (from ENG-5) and choose the authoritative
//!   reference source + lock state + estimated offset for the badge.
//!
//! Invariant #1 is re-asserted in `sysref_no_pacing.rs`: this informs the badge
//! only; it never paces the output clock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_engine::ptp::{LockState, ReferenceStatus};
use multiview_engine::sysref::{
    classify_system, NtpClockState, NtpQuery, NtpReading, NtpStatusFlags, ReferenceSelector,
    SelectedReference, SystemRefConfig, SystemRefTracker,
};
use multiview_overlay::clock::{RefSource, RefStatus};

/// A reading with the given STA_* bits, clock state, and estimated error (ns).
fn reading(status_bits: i32, state: NtpClockState, est_error_ns: i64) -> NtpReading {
    NtpReading {
        status: NtpStatusFlags::from_bits(status_bits),
        clock_state: state,
        est_error_ns,
        max_error_ns: est_error_ns * 4,
        offset_ns: 1_000,
    }
}

fn cfg() -> SystemRefConfig {
    // 100 us estimated-error tolerance for a lock; assume Locked when the
    // adjtimex read is unavailable (the deployment host is NTP-disciplined).
    SystemRefConfig {
        est_error_tolerance_ns: 100_000,
        assumed_when_unavailable: LockState::Locked,
    }
}

// ---- classification ------------------------------------------------------

#[test]
fn unsync_bit_classifies_freerun() {
    // STA_UNSYNC set ⇒ the kernel clock is NOT disciplined, regardless of error.
    let r = reading(NtpStatusFlags::STA_UNSYNC, NtpClockState::Ok, 5);
    assert_eq!(classify_system(&r, &cfg()), LockState::Freerun);
}

#[test]
fn synced_within_tolerance_classifies_locked() {
    // Synchronised (no UNSYNC bit), TIME_OK, error well under tolerance ⇒ Locked.
    let r = reading(0, NtpClockState::Ok, 20_000);
    assert_eq!(classify_system(&r, &cfg()), LockState::Locked);
}

#[test]
fn synced_but_over_tolerance_classifies_holdover() {
    // Synchronised but the estimated error exceeds the lock tolerance: the clock
    // is coasting on a degraded discipline ⇒ Holdover (not yet abandoned).
    let r = reading(0, NtpClockState::Ok, 500_000);
    assert_eq!(classify_system(&r, &cfg()), LockState::Holdover);
}

#[test]
fn time_error_state_classifies_holdover_even_within_tolerance() {
    // The kernel reporting TIME_ERROR (clock not synchronised / unset) while the
    // UNSYNC bit is somehow clear still degrades to Holdover, not Locked.
    let r = reading(0, NtpClockState::Error, 1);
    assert_eq!(classify_system(&r, &cfg()), LockState::Holdover);
}

#[test]
fn tolerance_boundary_is_inclusive_locked() {
    // Exactly at the tolerance counts as Locked (inclusive bound).
    let r = reading(0, NtpClockState::Ok, 100_000);
    assert_eq!(classify_system(&r, &cfg()), LockState::Locked);
}

// ---- the tracker (available + unavailable arms) --------------------------

/// An injectable NTP source that yields a queued reading, or `None` to model an
/// unavailable `adjtimex` (container without the cap, non-Linux, syscall error).
struct FakeNtp {
    next: Option<NtpReading>,
}

impl NtpQuery for FakeNtp {
    fn read(&mut self) -> Option<NtpReading> {
        self.next
    }
}

#[test]
fn tracker_uses_the_injected_reading_when_available() {
    let mut t = SystemRefTracker::new(cfg());
    let mut q = FakeNtp {
        next: Some(reading(0, NtpClockState::Ok, 10_000)),
    };
    assert_eq!(t.sample(&mut q), LockState::Locked);
    // The offset is surfaced for the badge.
    assert_eq!(t.offset_ns(), 1_000);
}

#[test]
fn tracker_falls_back_to_assumed_when_unavailable() {
    // No reading ⇒ default-safe fallback to the configured assumed state rather
    // than panicking or reporting a misleading Freerun.
    let mut t = SystemRefTracker::new(cfg());
    let mut q = FakeNtp { next: None };
    assert_eq!(t.sample(&mut q), LockState::Locked);
}

#[test]
fn tracker_fallback_freerun_is_honest_when_configured() {
    let mut t = SystemRefTracker::new(SystemRefConfig {
        est_error_tolerance_ns: 100_000,
        assumed_when_unavailable: LockState::Freerun,
    });
    let mut q = FakeNtp { next: None };
    assert_eq!(t.sample(&mut q), LockState::Freerun);
}

// ---- source selection (SYS vs PTP) ---------------------------------------

fn ptp(state: LockState, offset_ns: i64) -> ReferenceStatus {
    ReferenceStatus {
        state,
        offset_ns,
        frequency_ppb: 0,
        accepted: 100,
        disciplined: state.is_disciplined(),
    }
}

#[test]
fn ptp_wins_when_both_disciplined() {
    // PTP is the higher-stratum reference: when both PTP and SYS are disciplined,
    // PTP is authoritative (ST 2059 media-reference posture).
    let sel = ReferenceSelector::default();
    let chosen = sel.select(LockState::Locked, 1_000, &ptp(LockState::Locked, 250));
    assert_eq!(
        chosen,
        SelectedReference {
            source: RefSource::Ptp,
            state: LockState::Locked,
            offset_ns: 250,
        }
    );
}

#[test]
fn system_used_when_ptp_absent() {
    // No usable PTP (Freerun) ⇒ fall back to the system NTP discipline.
    let sel = ReferenceSelector::default();
    let chosen = sel.select(LockState::Locked, 1_000, &ptp(LockState::Freerun, 0));
    assert_eq!(chosen.source, RefSource::System);
    assert_eq!(chosen.state, LockState::Locked);
    assert_eq!(chosen.offset_ns, 1_000);
}

#[test]
fn ptp_holdover_still_preferred_over_system_locked() {
    // A coasting (Holdover) PTP reference is still disciplined and outranks a
    // locked system clock — the badge keeps showing PTP until it is abandoned.
    let sel = ReferenceSelector::default();
    let chosen = sel.select(LockState::Locked, 1_000, &ptp(LockState::Holdover, 9));
    assert_eq!(chosen.source, RefSource::Ptp);
    assert_eq!(chosen.state, LockState::Holdover);
}

#[test]
fn system_freerun_when_neither_disciplined() {
    // Neither reference disciplined ⇒ report the system source, Freerun (the
    // honest "no external discipline" badge).
    let sel = ReferenceSelector::default();
    let chosen = sel.select(LockState::Freerun, 0, &ptp(LockState::Freerun, 0));
    assert_eq!(chosen.source, RefSource::System);
    assert_eq!(chosen.state, LockState::Freerun);
}

#[test]
fn selected_reference_maps_to_overlay_ref_status() {
    // The selected lock state maps onto the overlay badge's RefStatus so the
    // clock can render text+glyph for all four states (a11y: never colour alone).
    let sel = ReferenceSelector::default();
    let chosen = sel.select(LockState::Locked, 0, &ptp(LockState::Holdover, 0));
    assert_eq!(chosen.to_ref_status(), RefStatus::Holdover);
    assert_eq!(chosen.to_time_ref().source, RefSource::Ptp);
    assert_eq!(chosen.to_time_ref().status, RefStatus::Holdover);
}
