//! One process-wide, bounded budget for player PCM decks, including the transient
//! decoded vamp window and every staged/retired/committed deck. Admission happens
//! before allocation; a lease is released only on actual Drop, never on request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Provisional hard ceiling for all player PCM across all (currently ≤2) slots.
/// This is NOT the VT decoded-memory-slot budget; that distinct configurable
/// budget lands with Memory storage. A 600 s vamp may be refused under this cap.
pub const PLAYER_PCM_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// Atomic admission and exact current reserved usage (not an upfront allocation).
pub struct AudioDeckBudget {
    cap: usize,
    used: AtomicUsize,
}

impl AudioDeckBudget {
    /// A budget with a fixed cap; no bytes allocated until reservations succeed.
    #[must_use]
    pub const fn new(cap: usize) -> Self {
        Self {
            cap,
            used: AtomicUsize::new(0),
        }
    }

    /// The current total in-use reservation in bytes.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Reserve the projected peak *before* creating a decoded window or deck.
    ///
    /// # Errors
    /// Returns `(requested, used, cap)` without changing usage on overflow or
    /// exhaustion. The caller retains its old playing clip unchanged.
    pub fn reserve(&'static self, bytes: usize) -> Result<AudioDeckLease, (usize, usize, usize)> {
        let mut used = self.used();
        loop {
            let Some(total) = used.checked_add(bytes) else {
                return Err((bytes, used, self.cap));
            };
            if total > self.cap {
                return Err((bytes, used, self.cap));
            }
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Ok(AudioDeckLease {
                        budget: self,
                        bytes,
                    })
                }
                Err(observed) => used = observed,
            }
        }
    }
}

/// A transfer-only budget lease. The retained `bytes` may be lowered once the
/// temporary decode window is freed, but never increased without admission.
pub struct AudioDeckLease {
    budget: &'static AudioDeckBudget,
    bytes: usize,
}

impl AudioDeckLease {
    /// Reduce projected peak to the actual retained deck allocation.
    ///
    /// # Errors
    /// Cannot increase the lease; obtain a larger reservation before allocation.
    pub fn shrink_to(&mut self, retained: usize) -> Result<(), &'static str> {
        if retained > self.bytes {
            return Err("retained audio bytes exceed reserved peak");
        }
        self.budget
            .used
            .fetch_sub(self.bytes - retained, Ordering::AcqRel);
        self.bytes = retained;
        Ok(())
    }

    /// Current bytes charged by this lease.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for AudioDeckLease {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Shared by every player; even aborted loads and retirees retain their leases
/// until their owning values are actually destroyed on the appropriate thread.
#[must_use]
pub fn player_audio_budget() -> &'static AudioDeckBudget {
    static BUDGET: OnceLock<AudioDeckBudget> = OnceLock::new();
    BUDGET.get_or_init(|| AudioDeckBudget::new(PLAYER_PCM_BUDGET_BYTES))
}

/// Serialises every test that charges or observes the real process-wide
/// [`player_audio_budget`], whose production cap and semantics stay untouched.
/// Hold the guard for the whole test; poisoning is ignored so one failure does
/// not cascade.
#[cfg(all(test, feature = "ffmpeg"))]
pub(crate) fn budget_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn old_and_new_coexist_and_refusal_does_not_evict() {
        static BUDGET: AudioDeckBudget = AudioDeckBudget::new(100);
        let old = BUDGET.reserve(40).unwrap();
        let mut new = BUDGET.reserve(60).unwrap();
        assert_eq!(BUDGET.reserve(1).err(), Some((1, 100, 100)));
        new.shrink_to(20).unwrap();
        assert_eq!(BUDGET.used(), 60);
        drop(old);
        assert_eq!(BUDGET.used(), 20);
        drop(new);
        assert_eq!(BUDGET.used(), 0);
    }

    #[test]
    fn simultaneous_admission_never_exceeds_cap() {
        static CONCURRENT: AudioDeckBudget = AudioDeckBudget::new(80);
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let lease = CONCURRENT.reserve(50);
                    barrier.wait();
                    lease.is_ok()
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        let wins = workers
            .into_iter()
            .map(|j| j.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
        assert_eq!(CONCURRENT.used(), 0);
    }

    #[test]
    fn arithmetic_overflow_fails_without_charging() {
        static SMALL: AudioDeckBudget = AudioDeckBudget::new(100);
        assert!(SMALL.reserve(usize::MAX).is_err());
        assert_eq!(SMALL.used(), 0);
    }
}
