//! The non-blocking latest-frame mailbox between the engine's canvas publish and
//! the display sink thread (invariants #1 + #10, ADR-0044).
//!
//! Built on `multiview-framestore`'s [`LatestSlot`] (the same non-blocking
//! overwrite/newest-wins primitive behind the per-tile last-good stores), with
//! one display-specific addition: the publish **sequence number travels inside
//! the slot value**, stamped atomically with the frame, so the reader can
//! never observe frame *N* paired with sequence *N+1*. The sequence is what
//! lets the flip loop decide "is there anything newer than what I last
//! committed?" without consuming or comparing payloads.
//!
//! The publisher side is one atomic counter bump plus a slot write that never
//! waits on the sink thread. That holds whether the sink is alive, slow,
//! wedged, gone, or descheduled in the middle of a read: a slot the sink is
//! inside is skipped, never waited for. The sink's read never blocks either.
//! The engine never waits on the sink here (#133: the slot no longer uses
//! arc-swap, which could hand this reader another mailbox's frame; see
//! vorner/arc-swap#210).

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

/// The engine-side handle: publishes the latest frame without waiting on the sink.
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
    /// Never waits on the sink thread, in any state (including descheduled
    /// mid-read): one atomic counter bump plus a slot write that skips any slot
    /// the sink is inside.
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
