//! The wait-free latest-frame mailbox between the engine's canvas publish and
//! the display sink thread (invariants #1 + #10, ADR-0044).
//!
//! Built on `multiview-framestore`'s [`LatestSlot`] (the same lock-free
//! overwrite/newest-wins primitive behind the per-tile last-good stores), with
//! one display-specific addition: the publish **sequence number travels inside
//! the slot value**, stamped atomically with the frame, so the reader can
//! never observe frame *N* paired with sequence *N+1*. The sequence is what
//! lets the flip loop decide "is there anything newer than what I last
//! committed?" without consuming or comparing payloads.
//!
//! The publisher side is a single atomic counter bump plus one lock-free
//! `Arc` swap — wait-free regardless of what the sink thread is doing (alive,
//! slow, wedged, or gone). The engine never blocks here.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_framestore::LatestSlot;

/// A frame with its publish sequence stamped alongside (one allocation per
/// publish; the payload itself is typically an `Arc`'d canvas clone).
#[derive(Debug)]
struct Stamped<F> {
    seq: u64,
    frame: F,
}

/// A frame handed out by [`FrameReader::latest`]: derefs to the payload and
/// keeps the underlying allocation alive while the sink works with it (a new
/// publish can land concurrently without invalidating this view).
#[derive(Debug)]
pub struct MailboxFrame<F>(Arc<Stamped<F>>);

impl<F> MailboxFrame<F> {
    /// The publish sequence stamped with this frame (strictly increasing).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.0.seq
    }
}

impl<F> Deref for MailboxFrame<F> {
    type Target = F;

    fn deref(&self) -> &F {
        &self.0.frame
    }
}

impl<F> Clone for MailboxFrame<F> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// The engine-side handle: publishes the latest frame, wait-free.
#[derive(Debug)]
pub struct FramePublisher<F> {
    slot: Arc<LatestSlot<Stamped<F>>>,
    counter: Arc<AtomicU64>,
}

impl<F> Clone for FramePublisher<F> {
    fn clone(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
            counter: Arc::clone(&self.counter),
        }
    }
}

impl<F> FramePublisher<F> {
    /// Publish `frame`, overwriting any unconsumed previous frame (newest
    /// wins). Returns the sequence number assigned to this publish.
    ///
    /// Wait-free: one atomic counter bump + one lock-free `Arc` swap. Never
    /// blocks, regardless of the sink thread's state.
    pub fn publish(&self, frame: F) -> u64 {
        let seq = self.counter.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.slot.publish(Stamped { seq, frame });
        seq
    }
}

/// The sink-side handle: peeks the latest frame without blocking.
#[derive(Debug)]
pub struct FrameReader<F> {
    slot: Arc<LatestSlot<Stamped<F>>>,
}

impl<F> FrameReader<F> {
    /// The latest published frame and its sequence, or [`None`] when nothing
    /// has ever been published. Never blocks; never consumes — the flip loop
    /// keeps the same frame as the retry candidate after an `EBUSY`.
    #[must_use]
    pub fn latest(&self) -> Option<(MailboxFrame<F>, u64)> {
        let stamped = self.slot.load()?;
        let seq = stamped.seq;
        Some((MailboxFrame(stamped), seq))
    }
}

/// Create a connected publisher/reader pair over one empty mailbox slot.
#[must_use]
pub fn frame_mailbox<F>() -> (FramePublisher<F>, FrameReader<F>) {
    let slot = Arc::new(LatestSlot::new());
    (
        FramePublisher {
            slot: Arc::clone(&slot),
            counter: Arc::new(AtomicU64::new(0)),
        },
        FrameReader { slot },
    )
}
