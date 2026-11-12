//! Output routing (ADR-0108 slice 1): the output clock's only new duty.
//!
//! The clock hands each FRESH composed tick to the off-clock `routing-aux`
//! thread through one non-blocking `try_send` into a capacity-4 channel, and
//! only while some routed feed is active ([`AuxTickSender::active_flag`]).
//! A full or disconnected channel drops the tick and counts it: the clock never
//! waits on the aux thread (invariants #1 and #10). See
//! `docs/architecture/output-routing.md` §2.2 and §4.4.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_core::time::MediaTime;

use crate::clock::Tick;
use crate::drive::{CompositedFrame, CompositorDrive};
use crate::runtime::FrameControl;

/// Capacity of the clock → aux tick channel (§2.2: 16-byte ticks, cap 4).
pub const AUX_TICK_CAPACITY: usize = 4;

/// One fresh output tick handed to the aux thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxTick {
    /// The output tick index.
    pub index: u64,
    /// The output tick's exact presentation time (the aux canvas reads at it).
    pub pts: MediaTime,
}

/// The clock side of the aux tick channel. Never blocks.
#[derive(Debug, Clone)]
pub struct AuxTickSender {
    tx: SyncSender<AuxTick>,
    active: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    sent: Arc<AtomicU64>,
}

/// Build the bounded clock → aux channel. The sender starts INACTIVE: no tick
/// is sent until a routed feed sets [`AuxTickSender::active_flag`].
#[must_use]
pub fn aux_tick_channel() -> (AuxTickSender, Receiver<AuxTick>) {
    let (tx, rx) = sync_channel(AUX_TICK_CAPACITY);
    (
        AuxTickSender {
            tx,
            active: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            sent: Arc::new(AtomicU64::new(0)),
        },
        rx,
    )
}

impl AuxTickSender {
    /// Offer one tick: a relaxed flag check, then at most one `try_send`.
    /// Full or disconnected → the tick is dropped and counted.
    pub fn offer(&self, tick: AuxTick) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        match self.tx.try_send(tick) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The flag the routing consumer raises while any routed feed is active.
    #[must_use]
    pub fn active_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.active)
    }

    /// Ticks handed to the aux thread (zero while no routed feed is active).
    #[must_use]
    pub fn sent_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.sent)
    }

    /// Ticks dropped because the aux thread was full, wedged or gone.
    #[must_use]
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

/// Wrap any [`FrameControl`] so a fresh composed tick is also offered to the
/// aux thread. Pure delegation otherwise. A repeat tick never reaches
/// `after_compose`, and that is correct: a repeat is not a new picture.
pub struct WithAuxTick<C> {
    /// The wrapped control, called exactly as without the wrapper.
    pub inner: C,
    /// The non-blocking tick sender.
    pub aux: AuxTickSender,
}

impl<C: FrameControl> FrameControl for WithAuxTick<C> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.before_compose(drive, tick);
    }

    fn after_compose(&mut self, frame: &CompositedFrame) {
        self.inner.after_compose(frame);
        self.aux.offer(AuxTick {
            index: frame.tick.index,
            pts: frame.tick.pts,
        });
    }

    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, tick: Tick) {
        self.inner.on_repeat(drive, tick);
    }
}

/// Public [`FrameControl`] adapter for a legacy per-boundary closure. Behaves
/// exactly like the runtime's private adapter behind
/// `run_with_control`: the closure runs before every compose and on every
/// repeat, and `after_compose` does nothing.
pub struct ClosureControl<F>(pub F);

impl<F: FnMut(&mut CompositorDrive<Nv12Image>)> FrameControl for ClosureControl<F> {
    fn before_compose(&mut self, drive: &mut CompositorDrive<Nv12Image>, _tick: Tick) {
        (self.0)(drive);
    }

    fn after_compose(&mut self, _frame: &CompositedFrame) {}

    fn on_repeat(&mut self, drive: &mut CompositorDrive<Nv12Image>, _tick: Tick) {
        (self.0)(drive);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn tick(i: u64) -> AuxTick {
        AuxTick {
            index: i,
            pts: MediaTime::from_nanos(i64::try_from(i).unwrap()),
        }
    }

    #[test]
    fn inactive_sender_sends_nothing() {
        let (tx, rx) = aux_tick_channel();
        tx.offer(tick(1));
        assert!(rx.try_recv().is_err());
        assert_eq!(tx.dropped_counter().load(Ordering::Relaxed), 0);
        assert_eq!(tx.sent_counter().load(Ordering::Relaxed), 0);
    }

    #[test]
    fn full_channel_drops_and_counts() {
        let (tx, rx) = aux_tick_channel();
        tx.active_flag().store(true, Ordering::Release);
        for i in 0..10 {
            tx.offer(tick(i));
        }
        assert_eq!(tx.dropped_counter().load(Ordering::Relaxed), 6);
        assert_eq!(tx.sent_counter().load(Ordering::Relaxed), 4);
        assert_eq!(rx.try_recv().unwrap().index, 0);
        drop(rx);
        tx.offer(tick(11));
        assert_eq!(tx.dropped_counter().load(Ordering::Relaxed), 7);
    }
}
