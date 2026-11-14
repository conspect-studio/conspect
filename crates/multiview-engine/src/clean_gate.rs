//! The one admission point for the Clean / first-DSK exclusion (ADR-0113
//! K2a deviation until pre-DSK Clean lands in K2b, #435; K4 review round 1
//! H3).
//!
//! Two facts must never both hold: "a downstream keyer is defined, or a
//! `keyer_set` defining one is in flight" and "an output carries Clean, or a
//! take to Clean is pending". Each side **counts** its reservations in one
//! packed atomic word, and every reservation is a compare-and-swap that
//! checks the other side in the same step:
//!
//! * a DSK reservation succeeds only while no Clean reservation is held;
//! * a Clean reservation succeeds only while no DSK is defined and no DSK
//!   reservation is held.
//!
//! Each reservation is an RAII guard that releases exactly its own count, so
//! a failing request can never clear a reservation another request still
//! holds (a bool would). The committed "defined" bit is set by the switcher
//! at the commit of a tick whose `keyer_set` defined a keyer (or at attach)
//! and is never cleared: K4 defines no verb that removes a keyer.
//!
//! Holders: the control plane (a `keyer_set` defining a new keyer, from
//! before its enqueue until its engine outcome settles), the switcher tick
//! (the same `keyer_set` again at the drain, a backstop for any caller), and
//! the output router (one Clean reservation per output whose bus is Clean or
//! whose pending take targets Clean). Every operation is a bounded
//! lock-free CAS on one word: no lock, no allocation, no wait (invariants
//! #1 and #10).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Bit 63: a downstream keyer is committed.
const DEFINED: u64 = 1 << 63;
/// Bits 0..32: outstanding DSK reservations.
const DSK_MASK: u64 = 0xFFFF_FFFF;
/// Bits 32..63: outstanding Clean reservations.
const CLEAN_SHIFT: u32 = 32;
const CLEAN_MASK: u64 = 0x7FFF_FFFF << CLEAN_SHIFT;
const CLEAN_ONE: u64 = 1 << CLEAN_SHIFT;

/// The shared Clean / DSK admission word. Cheap to share (`Arc`).
#[derive(Debug, Default)]
pub struct CleanDskGate {
    word: AtomicU64,
}

/// A held DSK reservation: while it lives, no Clean reservation can be
/// taken. Dropping it releases exactly this one count.
#[derive(Debug)]
#[must_use = "dropping the reservation releases it at once"]
pub struct DskReservation {
    gate: Arc<CleanDskGate>,
}

/// A held Clean reservation: while it lives, no DSK reservation can be
/// taken. Dropping it releases exactly this one count.
#[derive(Debug)]
#[must_use = "dropping the reservation releases it at once"]
pub struct CleanReservation {
    gate: Arc<CleanDskGate>,
}

impl CleanDskGate {
    /// An open gate: nothing defined, nothing reserved.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One CAS step: apply `f` to the current word, or stop when it answers
    /// `None`. Lock-free; retries only on a concurrent change.
    fn update(&self, f: impl Fn(u64) -> Option<u64>) -> bool {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let Some(next) = f(current) else {
                return false;
            };
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    /// Reserve "a DSK is being defined": refused while any Clean reservation
    /// is held (an output carries Clean or has a take to it pending).
    #[must_use]
    pub fn try_reserve_dsk(self: &Arc<Self>) -> Option<DskReservation> {
        self.update(|w| {
            (w & CLEAN_MASK == 0 && w & DSK_MASK != DSK_MASK).then_some(w + 1)
        })
        .then(|| DskReservation {
            gate: Arc::clone(self),
        })
    }

    /// Reserve "Clean is routed or a take to it is pending": refused while a
    /// DSK is defined or any DSK reservation is held.
    #[must_use]
    pub fn try_reserve_clean(self: &Arc<Self>) -> Option<CleanReservation> {
        self.update(|w| {
            (w & DEFINED == 0 && w & DSK_MASK == 0 && w & CLEAN_MASK != CLEAN_MASK)
                .then_some(w + CLEAN_ONE)
        })
        .then(|| CleanReservation {
            gate: Arc::clone(self),
        })
    }

    /// Mark a downstream keyer committed (attach, or the commit of a tick
    /// whose `keyer_set` defined one). Never cleared by a `keyer_set`.
    ///
    /// The switcher calls this while it still holds the tick's own DSK
    /// reservation (or at attach, before any Clean take), so no Clean
    /// reservation can be held here; the bit is set unconditionally either
    /// way (fail-safe: Clean is then refused).
    pub fn set_defined(&self) {
        self.word.fetch_or(DEFINED, Ordering::AcqRel);
    }

    /// Whether a downstream keyer is committed.
    #[must_use]
    pub fn defined(&self) -> bool {
        self.word.load(Ordering::Acquire) & DEFINED != 0
    }

    /// Whether a new Clean take would be refused now (a DSK is defined or
    /// being defined). Advisory (plan answers): only
    /// [`Self::try_reserve_clean`] admits.
    #[must_use]
    pub fn clean_refused(&self) -> bool {
        let w = self.word.load(Ordering::Acquire);
        w & DEFINED != 0 || w & DSK_MASK != 0
    }

    /// Outstanding DSK reservations.
    #[must_use]
    pub fn dsk_reservations(&self) -> u32 {
        u32::try_from(self.word.load(Ordering::Acquire) & DSK_MASK).unwrap_or(u32::MAX)
    }

    /// Outstanding Clean reservations.
    #[must_use]
    pub fn clean_reservations(&self) -> u32 {
        u32::try_from((self.word.load(Ordering::Acquire) & CLEAN_MASK) >> CLEAN_SHIFT)
            .unwrap_or(u32::MAX)
    }
}

impl Drop for DskReservation {
    fn drop(&mut self) {
        // Exactly this reservation's count; the CAS never underflows.
        let _ = self
            .gate
            .update(|w| (w & DSK_MASK != 0).then_some(w - 1));
    }
}

impl Drop for CleanReservation {
    fn drop(&mut self) {
        let _ = self
            .gate
            .update(|w| (w & CLEAN_MASK != 0).then_some(w - CLEAN_ONE));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn a_dsk_reservation_excludes_clean_and_counts_holders() {
        let gate = CleanDskGate::new();
        let a = gate.try_reserve_dsk().unwrap();
        let b = gate.try_reserve_dsk().unwrap();
        assert_eq!(gate.dsk_reservations(), 2);
        assert!(gate.try_reserve_clean().is_none());
        // B fails and releases: A still holds, Clean stays excluded.
        drop(b);
        assert_eq!(gate.dsk_reservations(), 1);
        assert!(gate.try_reserve_clean().is_none(), "A still holds");
        drop(a);
        let clean = gate.try_reserve_clean().unwrap();
        assert!(gate.try_reserve_dsk().is_none());
        drop(clean);
        assert!(gate.try_reserve_dsk().is_some());
    }

    #[test]
    fn a_defined_dsk_excludes_clean_for_good() {
        let gate = CleanDskGate::new();
        gate.set_defined();
        assert!(gate.clean_refused());
        assert!(gate.try_reserve_clean().is_none());
        // A further keyer may be defined (no Clean is held).
        assert!(gate.try_reserve_dsk().is_some());
        assert!(gate.defined());
    }

    /// Both orders race on one barrier per round: never both win.
    #[test]
    fn racing_reservations_are_mutually_exclusive() {
        let gate = CleanDskGate::new();
        for _ in 0..2_000 {
            let barrier = Arc::new(Barrier::new(2));
            let (g1, b1) = (Arc::clone(&gate), Arc::clone(&barrier));
            let dsk = std::thread::spawn(move || {
                b1.wait();
                g1.try_reserve_dsk()
            });
            barrier.wait();
            let clean = gate.try_reserve_clean();
            let dsk = dsk.join().unwrap();
            assert!(
                !(dsk.is_some() && clean.is_some()),
                "a DSK and a Clean reservation both won"
            );
            assert!(dsk.is_some() || clean.is_some(), "one side always wins");
        }
        assert_eq!(gate.dsk_reservations(), 0);
        assert_eq!(gate.clean_reservations(), 0);
    }
}
