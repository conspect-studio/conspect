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
        // Stamp the sequence first, then store. A reader that observes the new
        // `Arc` via `load` always sees a sequence >= the one stamped here
        // because `fetch_add` (AcqRel) happens-before the `store` (the store is
        // sequenced-after it in this thread). The exact pairing is asserted by
        // the concurrency test rather than relied upon for soundness — `Arc`
        // cloning is what guarantees no tearing.
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
