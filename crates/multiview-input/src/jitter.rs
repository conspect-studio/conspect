//! Bounded reorder / jitter buffer.
//!
//! Network transports reorder, duplicate, and delay packets. This buffer sorts
//! buffered items by presentation timestamp within a bounded window, releases
//! them in non-decreasing PTS order, and drops packets that arrive *after* their
//! release watermark has already passed (too late to be useful).
//!
//! It is **strictly bounded**: at capacity, a new in-order item evicts the
//! oldest (smallest-PTS) buffered item. The buffer therefore **drops, never
//! grows** — a load-bearing rule for the data plane (a bursting or flooding
//! input can never exhaust memory or back-pressure the engine).
use core::cmp::{Ordering, Reverse};
use multiview_core::time::MediaTime;
use std::collections::BinaryHeap;

/// The outcome of pushing an item into a [`ReorderBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReorderOutcome {
    /// The item was buffered for later release.
    Buffered,
    /// The item's PTS was at or before the last released watermark, so it
    /// arrived too late and was discarded.
    DroppedLate,
    /// The buffer was full, so the oldest (smallest-PTS) buffered item was
    /// evicted to make room for this (newer) item.
    DroppedToMakeRoom,
}

/// An entry in the reorder heap, ordered by `(pts, seq)` so duplicate PTS values
/// release in insertion order (stable) and the min-heap yields smallest PTS
/// first.
#[derive(Debug)]
struct Entry<T> {
    pts: MediaTime,
    seq: u64,
    value: T,
}

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.pts == other.pts && self.seq == other.seq
    }
}
impl<T> Eq for Entry<T> {}
impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.pts
            .cmp(&other.pts)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

/// A bounded, PTS-ordered reorder buffer.
///
/// `T` is the buffered payload (a packet, a decoded frame handle, …). Items are
/// keyed by their [`MediaTime`] presentation timestamp.
#[derive(Debug)]
pub struct ReorderBuffer<T> {
    /// Min-heap (via [`Reverse`]) keyed by `(pts, seq)`.
    heap: BinaryHeap<Reverse<Entry<T>>>,
    capacity: usize,
    next_seq: u64,
    /// The PTS of the most recently released item; pushes at or below it are too
    /// late.
    watermark: Option<MediaTime>,
}

impl<T> ReorderBuffer<T> {
    /// Create a buffer holding at most `capacity` items.
    ///
    /// A `capacity` of zero is clamped to one so the buffer can always hold the
    /// item it is currently servicing.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
            next_seq: 0,
            watermark: None,
        }
    }

    /// The configured capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of buffered items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether the buffer holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Push an item with presentation timestamp `pts`.
    ///
    /// Returns [`ReorderOutcome::DroppedLate`] if `pts` is at or before the last
    /// released watermark (too late). Otherwise the item is buffered; if the
    /// buffer is already full, the oldest (smallest-PTS) item is evicted to make
    /// room and [`ReorderOutcome::DroppedToMakeRoom`] is returned.
    pub fn push(&mut self, pts: MediaTime, value: T) -> ReorderOutcome {
        if let Some(w) = self.watermark {
            if pts <= w {
                return ReorderOutcome::DroppedLate;
            }
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let entry = Entry { pts, seq, value };

        if self.heap.len() < self.capacity {
            self.heap.push(Reverse(entry));
            return ReorderOutcome::Buffered;
        }

        // Full: evict the current oldest (smallest PTS) to make room. The heap
        // is a min-heap, so `peek` is the smallest. If the incoming item is
        // older than everything buffered, it is itself the one to drop.
        match self.heap.peek() {
            Some(Reverse(oldest)) if entry.cmp(oldest) == Ordering::Greater => {
                let _evicted = self.heap.pop();
                self.heap.push(Reverse(entry));
                ReorderOutcome::DroppedToMakeRoom
            }
            _ => ReorderOutcome::DroppedToMakeRoom,
        }
    }

    /// Remove and return the smallest-PTS buffered item, advancing the release
    /// watermark to its PTS.
    #[must_use = "the popped item is removed from the buffer"]
    pub fn pop(&mut self) -> Option<(MediaTime, T)> {
        let Reverse(entry) = self.heap.pop()?;
        self.watermark = Some(entry.pts);
        Some((entry.pts, entry.value))
    }

    /// Peek the smallest PTS currently buffered without removing it.
    #[must_use]
    pub fn peek_pts(&self) -> Option<MediaTime> {
        self.heap.peek().map(|Reverse(e)| e.pts)
    }

    /// Drain every buffered item whose PTS is at or before `up_to`, in
    /// non-decreasing PTS order. Items with a later PTS stay buffered for a
    /// subsequent drain. The release watermark advances to the last drained PTS.
    pub fn drain_ready(&mut self, up_to: MediaTime) -> DrainReady<'_, T> {
        DrainReady { buf: self, up_to }
    }
}

/// Iterator returned by [`ReorderBuffer::drain_ready`]. Yields buffered items in
/// non-decreasing PTS order while their PTS is at or before the watermark.
#[derive(Debug)]
pub struct DrainReady<'a, T> {
    buf: &'a mut ReorderBuffer<T>,
    up_to: MediaTime,
}

impl<T> Iterator for DrainReady<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self.buf.peek_pts() {
            Some(pts) if pts <= self.up_to => self.buf.pop().map(|(_, v)| v),
            _ => None,
        }
    }
}
