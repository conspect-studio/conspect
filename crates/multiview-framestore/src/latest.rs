//! A non-blocking single-slot "latest value" store.
//!
//! [`LatestSlot<T>`] is the primitive behind invariant #2's per-tile
//! last-good-frame store (see [`crate::tile`]). It holds at most one value with
//! an **overwrite / newest-wins** policy: a writer publishes the latest value
//! and any older pending value is dropped. A reader always observes the most
//! recently published value (or nothing) and never waits on another party's
//! progress.
//!
//! Reads return a fresh [`Arc<T>`] clone, so a value cannot be torn or freed
//! out from under a reader. An in-flight reader keeps its `Arc` alive even as a
//! new value is published over the top.
//!
//! The slot is the textbook SPSC handoff (one decoder writes, one compositor
//! reads), but it is fully thread-safe, so concurrent writers and readers are
//! also sound. They simply race to publish and observe, which is exactly what
//! a live multiview wants.
//!
//! ## Storage, and why it is not `arc-swap` (#133)
//!
//! The value lives in a crate-private `SwapCell` (the `swap` module): a small
//! ring of `std::sync::RwLock` slots plus one atomic, generation-tagged head
//! index. A publish writes into a slot it can `try_write` (skipping any slot a
//! reader is inside, and growing the ring if all are) and then advances the
//! head, so **a publish never waits on a reader**, even one the OS has
//! descheduled mid-read. A load `try_read`s the head's slot and accepts it only
//! if its generation tag matches; it never blocks on a lock. Nothing is
//! dropped under a lock. See the `swap` module docs for the full argument.
//!
//! The slot used `arc_swap::ArcSwapOption` until #133. arc-swap 1.9.2's default
//! (hybrid) strategy can make `load_full` on one instance return an `Arc`
//! that was stored into a **different** instance. Its process-global debt list
//! is keyed by raw address, so when a freed value's address is reused by
//! another instance's store, that store's writer can "pay" this reader's
//! in-flight debt. The reader then accepts a pointer it never protected
//! (upstream [vorner/arc-swap#210], open). On macOS ARM in CI this surfaced as
//! `sequence regressed: 6613 < 7357`. That was not a coherence violation on
//! the slot's own pointer. It was another mailbox's frame. A reader-side
//! sequence guard cannot repair this: a foreign value may carry a *higher*
//! sequence, and with a different payload type it is type confusion. So the
//! storage was replaced. See the ADR-T002 addendum.
//!
//! [vorner/arc-swap#210]: https://github.com/vorner/arc-swap/issues/210
//!
//! ## Ordering guarantee (newest-wins, never-regresses, never-foreign)
//!
//! * **Never foreign.** [`LatestSlot::load`] returns exactly the `Arc` most
//!   recently stored into *this* slot. There is no reclamation state shared
//!   between slots.
//! * **Newest-wins, never lost.** A publish becomes visible when it advances
//!   the head (`fetch_max` on one atomic). A load returns the value the head
//!   names when it reads it. The head's value is never overwritten, so the
//!   newest value stays readable with no follow-up publish.
//! * **Never regresses.** A single reader's successive loads read the head
//!   atomic in program order, and coherence of one atomic location means it
//!   never moves backwards. So with a single writer, a reader can never get a
//!   value older than one it has already returned, on x86 or `AArch64`.
//! * **Progress.** Publish never waits on a reader (wait-free with respect to
//!   readers). Load never blocks on a lock and retries only when a publisher
//!   made progress (lock-free).
//!
//! These properties are exercised under stress by `tests/concurrency.rs`
//! (`reader_never_observes_a_regression_in_sequence`,
//! `load_never_returns_a_value_published_into_another_slot`).
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::swap::SwapCell;

/// A non-blocking, single-slot store holding the latest published value of type
/// `T`.
///
/// Publishing overwrites any previous value (newest wins). Reading clones the
/// current [`Arc<T>`] and never blocks; publishing never waits on a reader.
/// A monotonically increasing **sequence
/// number** is stamped on every publish so a reader can cheaply detect whether
/// a newer value has appeared since it last looked (used by the concurrency
/// test to assert "never older-than-seen").
#[derive(Debug)]
pub struct LatestSlot<T> {
    slot: SwapCell<Option<Arc<T>>>,
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
            slot: SwapCell::new(None),
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
        // 1. Newest-wins and no-regression for the *value* itself rest on the
        //    slot's atomic head (see the module-level "Ordering guarantee"
        //    docs), NOT on this seq counter. Cloning the `Arc` on load is what
        //    rules out tearing.
        // 2. The seq counter is a cheap staleness signal for callers that hold a
        //    value and want to know whether a newer one exists without comparing
        //    payloads (see [`LatestSlot::sequence`]). Stamping before the store
        //    means a reader that has *observed this value* and then reads the
        //    counter sees a number >= `next`. The `AcqRel` fetch_add is
        //    sequenced before the release RMW on the head that publishes the
        //    value, and a reader's acquire load of the head synchronizes with
        //    it.
        let next = self.seq.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        // The previous value (if any) is dropped outside any lock.
        self.slot.store(Some(value));
        next
    }

    /// Load the latest value, if any, as a cloned [`Arc<T>`].
    ///
    /// Never blocks on a lock (lock-free; see the module docs). The only
    /// critical section is one `Arc` clone. Returns [`None`] only when nothing has ever been published
    /// (or the slot was explicitly [cleared](LatestSlot::take)).
    #[must_use]
    pub fn load(&self) -> Option<Arc<T>> {
        self.slot.load()
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
