//! A lock-free single-slot "latest value" store.
//!
//! [`LatestSlot<T>`] is the primitive behind invariant #2's per-tile
//! last-good-frame store (see [`crate::tile`]). It holds at most one value with
//! an **overwrite / newest-wins** policy: a writer publishes the latest value
//! and an older pending value is dropped; a reader always observes the most
//! recently published value (or nothing) and **never blocks**.
//!
//! It is implemented on top of [`arc_swap::ArcSwapOption`] — a safe, well-tested
//! lock-free atomic `Arc` slot — rather than any hand-rolled `unsafe`. Reads
//! return a fresh [`Arc<T>`] clone, so the value cannot be torn or freed out
//! from under a reader: an in-flight reader keeps its `Arc` alive even as a new
//! value is published over the top.
//!
//! The slot is the textbook SPSC handoff (one decoder writes, one compositor
//! reads), but `ArcSwapOption` is fully thread-safe, so concurrent writers and
//! readers are also sound — they simply race to publish/observe, exactly the
//! semantics a live multiview wants.
//!
//! ## Ordering guarantee (newest-wins, never-regresses)
//!
//! The value lives in a **single atomic pointer** inside the `ArcSwapOption`.
//! That single location is what makes invariant #2's contract sound, and the
//! argument rests on the C++/Rust memory model rather than on any barrier we
//! place by hand:
//!
//! * **Single total modification order.** All writes to one atomic object occur
//!   in one total order (`[intro.races]/4`) — for a single writer that order is
//!   exactly its program order of `store`s. This holds for *every* ordering,
//!   down to `Relaxed`, on *every* architecture (it is the one guarantee weak
//!   memory like `AArch64` always provides — what ARM relaxes is ordering
//!   *across different* locations, never the per-location order).
//! * **Read-read coherence.** Two loads of one atomic object that are
//!   sequenced-before one another (i.e. the repeated loads of a single reader)
//!   may not move *backwards* in that modification order (`[intro.races]/12`):
//!   a load takes its value from the one it last read or a *later* write, never
//!   an earlier one.
//!
//! Together these mean a single reader looping on [`LatestSlot::load`] can never
//! be handed a value older than one it has already returned — there is no
//! sequence regression, on x86 or ARM, regardless of the atomic ordering used.
//! `arc-swap` in fact stores with `SeqCst` and loads with `Acquire` (its
//! `swap` uses `SeqCst`; the hybrid load path is `Acquire` on the pointer), so
//! the property holds with room to spare; its debt-list / hazard-pointer
//! fallback only governs safe `Arc` reclamation, never *which* pointer value a
//! load returns. See arc-swap's internal design docs and `[intro.races]`
//! (<https://eel.is/c++draft/intro.races>). This is exercised empirically by
//! `tests/concurrency.rs::reader_never_observes_a_regression_in_sequence`.
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;

/// A lock-free, single-slot store holding the latest published value of type
/// `T`.
///
/// Publishing overwrites any previous value (newest wins); reading clones the
/// current [`Arc<T>`] without blocking. A monotonically increasing **sequence
/// number** is stamped on every publish so a reader can cheaply detect whether
/// a newer value has appeared since it last looked (used by the concurrency
/// test to assert "never older-than-seen").
#[derive(Debug)]
pub struct LatestSlot<T> {
    slot: ArcSwapOption<T>,
    seq: AtomicU64,
}

impl<T> Default for LatestSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LatestSlot<T> {
    /// Create an empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: ArcSwapOption::empty(),
            seq: AtomicU64::new(0),
        }
    }

    /// Create a slot pre-populated with `value` (sequence number `1`).
    #[must_use]
    pub fn with_value(value: T) -> Self {
        let this = Self::new();
        this.publish(value);
        this
    }

    /// Publish `value`, replacing (and dropping) any previous value.
    ///
    /// Returns the **sequence number** assigned to this publish. Sequence
    /// numbers are strictly increasing across all publishes to this slot, so a
    /// larger number always denotes a strictly newer value.
    pub fn publish(&self, value: T) -> u64 {
        self.publish_arc(Arc::new(value))
    }

    /// Publish an already-`Arc`-wrapped `value`, avoiding a second allocation
    /// when the caller already holds an [`Arc<T>`].
    ///
    /// Returns the assigned sequence number (see [`LatestSlot::publish`]).
    pub fn publish_arc(&self, value: Arc<T>) -> u64 {
        // Stamp the sequence first, then store the value. Two independent facts:
        //
        // 1. Newest-wins + no-regression for the *value* itself rests entirely
        //    on the single atomic pointer inside `slot` (read-read coherence —
        //    see the module-level "Ordering guarantee" docs), NOT on this seq
        //    counter. `Arc` cloning on load is what guarantees no tearing.
        // 2. The seq counter is a cheap staleness signal for callers that hold a
        //    value and want to know whether a newer one exists without comparing
        //    payloads (see [`LatestSlot::sequence`]). Stamping before the store
        //    means a reader that has *observed this value* and then reads the
        //    counter sees a number >= `next`: the `AcqRel` fetch_add is
        //    sequenced-before the `SeqCst` store in this thread, so any thread
        //    that sees the store also sees the bumped counter.
        let next = self.seq.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.slot.store(Some(value));
        next
    }

    /// Load the latest value, if any, as a cloned [`Arc<T>`].
    ///
    /// Never blocks. Returns [`None`] only when nothing has ever been published
    /// (or the slot was explicitly [cleared](LatestSlot::take)).
    #[must_use]
    pub fn load(&self) -> Option<Arc<T>> {
        self.slot.load_full()
    }

    /// The sequence number of the most recent publish (`0` if never published).
    ///
    /// Strictly increasing per publish; lets a reader detect staleness of a
    /// previously held value without comparing payloads.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Whether the slot currently holds no value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slot.load().is_none()
    }

    /// Atomically remove and return the current value, leaving the slot empty.
    ///
    /// Does not reset the sequence counter (sequence numbers remain strictly
    /// increasing for the life of the slot).
    pub fn take(&self) -> Option<Arc<T>> {
        self.slot.swap(None)
    }
}
