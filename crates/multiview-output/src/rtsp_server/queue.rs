//! The bounded **drop-oldest** packet buffer that decouples a slow/absent RTSP
//! client from the engine (invariants #1/#10).
//!
//! Each registered RTSP client (and the feed into `appsrc`) reads from one of
//! these. The producer side is the engine driving the encode-once fan-out on the
//! output clock: it pushes a packet reference per tick and **must never block**.
//! When the buffer is full the *oldest* queued packet is evicted to make room for
//! the newest — a fresh or recovering client must get current frames, never a
//! stale backlog (streaming-gotchas §4: never flush a backlog to a server).
//!
//! This is a pure-Rust, always-compiled building block: it carries no `GStreamer`
//! dependency and is fully CI-testable without the `rtsp-server` feature.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::fanout::EncodedPacket;

/// A bounded, drop-oldest queue of encoded packets shared between the engine
/// (producer, on the output clock) and an RTSP serving consumer.
///
/// `push` is non-blocking and bounded: it can be called from the hot path
/// without risk of stalling. When the queue is at capacity, the oldest packet is
/// dropped so the newest is retained.
#[derive(Debug)]
pub struct BoundedPacketQueue {
    inner: Mutex<VecDeque<Arc<EncodedPacket>>>,
    capacity: usize,
}

impl BoundedPacketQueue {
    /// Create a queue holding at most `capacity` packets.
    ///
    /// # Panics
    ///
    /// Never panics. A `capacity` of `0` is clamped to `1` so the queue can
    /// always hold the newest packet; prefer [`try_new`](Self::try_new) when a
    /// zero capacity should be a typed error rather than clamped.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Create a queue holding at most `capacity` packets, returning `None` for a
    /// zero capacity (which could never hold a packet).
    #[must_use]
    pub fn try_new(capacity: usize) -> Option<Self> {
        if capacity == 0 {
            return None;
        }
        Some(Self::new(capacity))
    }

    /// The configured maximum number of packets.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Enqueue a packet reference. **Never blocks and never grows past
    /// capacity:** if the queue is full the oldest packet is evicted first.
    ///
    /// Takes the packet by shared reference and clones the `Arc` (a refcount
    /// bump, not a payload copy) so the same encoded allocation fans out to every
    /// sink (encode-once, invariant #7).
    ///
    /// Returns `true` if a packet had to be dropped to make room (a slow/absent
    /// consumer signal the caller may surface as a shed metric), `false`
    /// otherwise.
    pub fn push(&self, packet: &Arc<EncodedPacket>) -> bool {
        // A poisoned lock means a consumer thread panicked while holding it; the
        // queue contents are still structurally valid, so recover the guard and
        // continue rather than propagating a panic onto the engine hot path.
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut dropped = false;
        while guard.len() >= self.capacity {
            // Evict oldest; the loop also corrects any over-fill from a prior
            // capacity change (defensive — capacity is fixed today).
            if guard.pop_front().is_some() {
                dropped = true;
            } else {
                break;
            }
        }
        guard.push_back(Arc::clone(packet));
        dropped
    }

    /// Remove and return the oldest queued packet, or `None` if empty. Never
    /// blocks (a drained queue yields `None` immediately).
    #[must_use]
    pub fn pop(&self) -> Option<Arc<EncodedPacket>> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.pop_front()
    }

    /// The current number of queued packets (`<= capacity`).
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.len()
    }

    /// Whether the queue currently holds no packets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain every queued packet in FIFO order, leaving the queue empty.
    #[must_use]
    pub fn drain(&self) -> Vec<Arc<EncodedPacket>> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.drain(..).collect()
    }
}
