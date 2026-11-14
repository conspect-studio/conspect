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
//! whose pending take targets Clean).
//!
//! Wait-freedom on the clock (review round 2 J2): a lock-free CAS loop is not
//! wait-free; concurrent control-plane reservations can keep invalidating
//! the clock's CAS. So the clock admits only through the `tick_*` entry
//! points, which make at most [`MAX_TICK_ATTEMPTS`] CAS attempts and then
//! refuse as [`TickRefusal::Contended`] (counted, [`CleanDskGate::contended`]).
//! Every release is one non-retrying `fetch_sub` on its own count field (the
//! count is at least the guard's own one, so it never borrows into the
//! neighbouring field), and `set_defined` is one `fetch_or`. Off-clock
//! callers (the control plane) may retry until the word settles. No lock,
//! no allocation (invariants #1 and #10).

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

/// CAS attempts a clock-side admission makes before it refuses as
/// contention (review J2): bounded work per tick, whatever other threads do.
pub const MAX_TICK_ATTEMPTS: u32 = 4;

/// Why a clock-side admission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickRefusal {
    /// The other side holds a reservation (or a DSK is defined).
    Excluded,
    /// Concurrent reservations kept changing the word for
    /// [`MAX_TICK_ATTEMPTS`] attempts; refused rather than spin (counted).
    Contended,
}

/// The shared Clean / DSK admission word. Cheap to share (`Arc`).
#[derive(Debug, Default)]
pub struct CleanDskGate {
    word: AtomicU64,
    /// Clock-side admissions refused as [`TickRefusal::Contended`].
    contended: AtomicU64,
}

fn dsk_step(w: u64) -> Option<u64> {
    (w & CLEAN_MASK == 0 && w & DSK_MASK != DSK_MASK).then_some(w + 1)
}

fn clean_step(w: u64) -> Option<u64> {
    (w & DEFINED == 0 && w & DSK_MASK == 0 && w & CLEAN_MASK != CLEAN_MASK).then_some(w + CLEAN_ONE)
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

    /// Off-clock admission: apply `f` to the current word, or stop when it
    /// answers `None`. Lock-free; retries only on a concurrent change. Never
    /// called on the clock (use [`Self::bounded`]).
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

    /// Clock-side admission: at most [`MAX_TICK_ATTEMPTS`] CAS attempts,
    /// then [`TickRefusal::Contended`] (counted). `between` runs before each
    /// attempt (a test seam; a no-op on the clock).
    fn bounded(
        &self,
        f: impl Fn(u64) -> Option<u64>,
        mut between: impl FnMut(),
    ) -> Result<(), TickRefusal> {
        let mut current = self.word.load(Ordering::Acquire);
        for _ in 0..MAX_TICK_ATTEMPTS {
            let Some(next) = f(current) else {
                return Err(TickRefusal::Excluded);
            };
            between();
            match self
                .word
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(seen) => current = seen,
            }
        }
        self.contended.fetch_add(1, Ordering::Relaxed);
        Err(TickRefusal::Contended)
    }

    /// Reserve "a DSK is being defined" (off the clock): refused while any
    /// Clean reservation is held (an output carries Clean or has a take to
    /// it pending).
    #[must_use]
    pub fn try_reserve_dsk(self: &Arc<Self>) -> Option<DskReservation> {
        self.update(dsk_step).then(|| DskReservation {
            gate: Arc::clone(self),
        })
    }

    /// Reserve "Clean is routed or a take to it is pending" (off the
    /// clock): refused while a DSK is defined or any DSK reservation is held.
    #[must_use]
    pub fn try_reserve_clean(self: &Arc<Self>) -> Option<CleanReservation> {
        self.update(clean_step).then(|| CleanReservation {
            gate: Arc::clone(self),
        })
    }

    /// [`Self::try_reserve_dsk`] on the clock: bounded work, refused as
    /// [`TickRefusal::Contended`] rather than spin (review J2).
    ///
    /// # Errors
    /// [`TickRefusal::Excluded`] while Clean is held; [`TickRefusal::Contended`]
    /// after [`MAX_TICK_ATTEMPTS`] lost CAS attempts.
    pub fn tick_reserve_dsk(self: &Arc<Self>) -> Result<DskReservation, TickRefusal> {
        self.tick_reserve_dsk_with(|| {})
    }

    fn tick_reserve_dsk_with(
        self: &Arc<Self>,
        between: impl FnMut(),
    ) -> Result<DskReservation, TickRefusal> {
        self.bounded(dsk_step, between).map(|()| DskReservation {
            gate: Arc::clone(self),
        })
    }

    /// [`Self::try_reserve_clean`] on the clock: bounded work, refused as
    /// [`TickRefusal::Contended`] rather than spin (review J2).
    ///
    /// # Errors
    /// [`TickRefusal::Excluded`] while a DSK is defined or reserved;
    /// [`TickRefusal::Contended`] after [`MAX_TICK_ATTEMPTS`] lost attempts.
    pub fn tick_reserve_clean(self: &Arc<Self>) -> Result<CleanReservation, TickRefusal> {
        self.bounded(clean_step, || {}).map(|()| CleanReservation {
            gate: Arc::clone(self),
        })
    }

    /// Clock-side admissions refused as contention so far.
    #[must_use]
    pub fn contended(&self) -> u64 {
        self.contended.load(Ordering::Relaxed)
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
        // One non-retrying op (review J2). This guard's own count is in the
        // DSK field, so it is at least 1 and the subtraction never borrows
        // into the Clean field.
        self.gate.word.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for CleanReservation {
    fn drop(&mut self) {
        // As above: at least this guard's count is in the Clean field, so it
        // never borrows into the defined bit.
        self.gate.word.fetch_sub(CLEAN_ONE, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::Barrier;

    /// Review J2: the clock's admission makes a bounded number of CAS
    /// attempts. A concurrent reservation lands between every load and CAS
    /// (deterministic, through the attempt seam) for far longer than the
    /// bound: the tick refuses as contention within the bound, counted, and
    /// never spins.
    #[test]
    fn tick_admission_is_bounded_under_contention_and_counted() {
        let gate = CleanDskGate::new();
        let attempts = std::cell::Cell::new(0_u32);
        let interfere = || {
            attempts.set(attempts.get() + 1);
            if attempts.get() <= 1_000 {
                // Another DSK holder reserves: the tick's DSK admission
                // stays allowed, but its CAS loses.
                gate.word.fetch_add(1, Ordering::AcqRel);
            }
        };
        let admitted = gate.tick_reserve_dsk_with(interfere);
        assert!(
            attempts.get() <= MAX_TICK_ATTEMPTS,
            "{} CAS attempts on the clock",
            attempts.get()
        );
        assert_eq!(admitted.err(), Some(TickRefusal::Contended));
        assert_eq!(gate.contended(), 1, "the contention refusal is counted");
    }

    /// The clock's bounded admission keeps the exclusion: excluded is not
    /// contention (not counted), and an uncontended word admits at once.
    /// Releases are single `fetch_sub`s that leave the other field intact.
    #[test]
    fn tick_admission_keeps_the_exclusion_and_releases_exactly_its_count() {
        let gate = CleanDskGate::new();
        let clean = gate.tick_reserve_clean().unwrap();
        assert_eq!(gate.tick_reserve_dsk().err(), Some(TickRefusal::Excluded));
        let other = gate.try_reserve_clean().unwrap();
        drop(clean);
        assert_eq!(gate.clean_reservations(), 1);
        assert_eq!(gate.tick_reserve_dsk().err(), Some(TickRefusal::Excluded));
        drop(other);
        let dsk = gate.tick_reserve_dsk().unwrap();
        assert_eq!(gate.tick_reserve_clean().err(), Some(TickRefusal::Excluded));
        gate.set_defined();
        drop(dsk);
        assert_eq!(gate.dsk_reservations(), 0);
        assert!(gate.defined(), "a release never touches the defined bit");
        assert_eq!(gate.contended(), 0);
    }

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
