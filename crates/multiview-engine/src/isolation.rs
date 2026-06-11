//! Outbound **isolation** wiring (invariant #10).
//!
//! The engine publishes its state and telemetry to control, preview, and
//! realtime consumers **only** through channels whose publish path is
//! *physically incapable* of being blocked by any consumer:
//!
//! * **Latest-state slot** ([`LatestState`]): a single-slot, newest-wins cell
//!   backed by [`arc_swap::ArcSwapOption`] (the same primitive
//!   `multiview-framestore` uses). The publisher's [`LatestState::publish`] is a
//!   single atomic `store` — **wait-free**: it acquires no lock, never spins on
//!   a reader, and cannot be made to wait by any number of concurrent readers. A
//!   consumer that never reads simply misses intermediate snapshots; it can
//!   never make the publisher wait.
//! * **Event stream** ([`EventStream`] / [`EventSubscription`]): a fixed-capacity
//!   ring built on [`tokio::sync::broadcast`]. The publisher's
//!   [`EventStream::publish`] calls `broadcast::Sender::send`, which **never
//!   awaits and never blocks on a slow or absent receiver**. When a subscriber
//!   falls behind, the channel overwrites the oldest buffered events
//!   (drop-oldest) and that subscriber observes a [`RecvError::Lagged`] reporting
//!   how many it missed — it never back-pressures the sender. Every event
//!   carries a strictly increasing **sequence number** so a reconnecting
//!   consumer can detect gaps and resume.
//!
//! Crucially there is **no consumer-callable method that takes a lock the
//! publisher also needs**. The previous implementation guarded a `VecDeque` ring
//! with a `std::sync::Mutex` shared between the publisher's `publish()` and the
//! consumers' `try_recv()`/`drain()`/`pending()`; a consumer holding (or
//! preempted while holding) that lock would stall the engine's publish path,
//! violating invariant #10. That mutex ring has been removed entirely.
//!
//! The chaos property the tests prove: a subscriber that never reads (or reads
//! arbitrarily slowly, or holds any lock it can reach, or crashes) **cannot**
//! slow the publisher. `publish` on either channel completes in bounded time
//! regardless of consumer behaviour; slow consumers lag and lose the oldest
//! items rather than blocking the engine or growing memory without bound.
use std::sync::Arc;

use multiview_framestore::LatestSlot;
use tokio::sync::broadcast;

pub use tokio::sync::broadcast::error::RecvError;
pub use tokio::sync::broadcast::error::TryRecvError;

/// A latest-state publisher: a lock-free, newest-wins single slot.
///
/// The engine publishes a snapshot (e.g. the current set of tile states, the
/// degradation level) every tick or every few ticks; a control/preview consumer
/// reads the latest whenever it can. A stalled consumer never blocks the engine
/// — it just observes a more recent snapshot when it next reads.
///
/// This realizes `tokio::sync::watch`-style "newest-wins" semantics over the
/// same lock-free [`arc_swap::ArcSwapOption`] primitive `multiview-framestore`
/// uses. The publish side is **wait-free**: a single atomic store with **no
/// `.await`**, **no lock**, and no observation of consumer progress.
#[derive(Debug)]
pub struct LatestState<T> {
    slot: Arc<LatestSlot<T>>,
}

impl<T> Clone for LatestState<T> {
    fn clone(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
        }
    }
}

impl<T> Default for LatestState<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LatestState<T> {
    /// Create an empty latest-state cell.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Arc::new(LatestSlot::new()),
        }
    }

    /// Publish the newest state, overwriting any previous value.
    ///
    /// **Wait-free**: a single atomic store. Returns the assigned monotonic
    /// sequence number. The engine calls this on the output clock and is
    /// guaranteed it cannot stall here — no consumer can block it.
    pub fn publish(&self, value: T) -> u64 {
        self.slot.publish(value)
    }

    /// Publish an already-`Arc`-wrapped value, avoiding a second allocation.
    ///
    /// **Wait-free** (see [`LatestState::publish`]). Returns the assigned
    /// monotonic sequence number.
    pub fn publish_arc(&self, value: Arc<T>) -> u64 {
        self.slot.publish_arc(value)
    }

    /// Read the latest published state, if any (a fresh `Arc` clone).
    ///
    /// Never blocks the publisher; returns [`None`] only when nothing has been
    /// published yet.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<T>> {
        self.slot.load()
    }

    /// The monotonic sequence number of the most recent publish (`0` if none).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.slot.sequence()
    }
}

/// An event tagged with the monotonic sequence number it was published at.
///
/// The sequence number is strictly increasing across the channel's lifetime, so
/// a reconnecting consumer can detect gaps (a jump greater than `+1`, or a
/// [`RecvError::Lagged`]) and resume / resynchronize.
#[derive(Debug)]
pub struct SeqEvent<T> {
    /// The strictly-increasing sequence number assigned at publish time.
    pub seq: u64,
    /// The payload.
    pub event: Arc<T>,
}

// Hand-implemented (not derived) so the bound is `Arc<T>: Clone` — always true —
// rather than the derive macro's spurious `T: Clone`. This lets the broadcast
// receiver clone/`resubscribe` even when `T` itself is not `Clone`.
impl<T> Clone for SeqEvent<T> {
    fn clone(&self) -> Self {
        Self {
            seq: self.seq,
            event: Arc::clone(&self.event),
        }
    }
}

/// The publisher half of a bounded **drop-oldest** event broadcast.
///
/// Built on [`tokio::sync::broadcast`]: [`EventStream::publish`] calls
/// `broadcast::Sender::send`, which **never awaits and never blocks** on slow or
/// absent receivers. When the ring is full for a lagging subscriber, the oldest
/// buffered events are overwritten (drop-oldest) and that subscriber sees a
/// [`RecvError::Lagged`] on its next receive — it can never back-pressure the
/// publisher.
///
/// Cloneable so the engine can hand publish handles to multiple producers; all
/// clones publish into the same channel and share the sequence counter.
#[derive(Debug)]
pub struct EventStream<T> {
    inner: Arc<broadcast::Sender<SeqEvent<T>>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
    capacity: usize,
}

impl<T> Clone for EventStream<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            seq: Arc::clone(&self.seq),
            capacity: self.capacity,
        }
    }
}

/// The consumer half: a live subscription to the event broadcast.
///
/// A subscription holds a `broadcast::Receiver`; receiving exerts **no
/// back-pressure** on the publisher. A subscriber that drains too slowly
/// observes [`RecvError::Lagged`] (it missed that many drop-oldest events) and
/// then continues from the oldest still-buffered event. No method here takes a
/// lock the publisher needs.
#[derive(Debug)]
pub struct EventSubscription<T> {
    rx: broadcast::Receiver<SeqEvent<T>>,
}

/// Create a bounded drop-oldest event broadcast with room for `capacity`
/// buffered events per subscriber, returning the publisher half and one
/// subscription.
///
/// A `capacity` of `0` is promoted to `1` (`tokio::sync::broadcast` requires a
/// positive capacity; the minimum useful ring buffers one event).
#[must_use]
pub fn event_stream<T>(capacity: usize) -> (EventStream<T>, EventSubscription<T>) {
    let capacity = capacity.max(1);
    let (tx, rx) = broadcast::channel::<SeqEvent<T>>(capacity);
    let stream = EventStream {
        inner: Arc::new(tx),
        seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        capacity,
    };
    (stream, EventSubscription { rx })
}

impl<T> EventStream<T> {
    /// Publish an event. **Non-blocking, bounded-time, never awaits or blocks on
    /// a consumer.**
    ///
    /// Internally a single `broadcast::Sender::send`. If every subscriber's ring
    /// is full the oldest buffered events are dropped (drop-oldest) and the
    /// laggards observe [`RecvError::Lagged`] on their next receive. Returns the
    /// strictly-increasing sequence number assigned to this event.
    ///
    /// A `send` with no live subscribers is **not** an error here — the engine
    /// publishes unconditionally and never observes whether anyone is listening
    /// (invariant #10).
    pub fn publish(&self, value: T) -> u64 {
        let seq = self
            .seq
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .wrapping_add(1);
        // `send` returns `Err` only when there are zero receivers; that is a
        // perfectly fine state for the engine (no listeners) and must never
        // affect the publish path. We deliberately ignore it.
        let _ = self.inner.send(SeqEvent {
            seq,
            event: Arc::new(value),
        });
        seq
    }

    /// Obtain a new subscription that will receive events published *after* this
    /// call.
    ///
    /// A late subscriber does not see history that predates it; it resumes from
    /// the current sequence position and detects any subsequent gap via the
    /// per-event [`SeqEvent::seq`].
    #[must_use]
    pub fn subscribe(&self) -> EventSubscription<T> {
        EventSubscription {
            rx: self.inner.subscribe(),
        }
    }

    /// The sequence number of the most recently published event (`0` if none).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The number of subscribers currently connected to this stream.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner.receiver_count()
    }

    /// The per-subscriber ring capacity (the drop-oldest depth).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl<T> EventSubscription<T> {
    /// Await the next event. Cooperative and cancel-safe.
    ///
    /// Returns [`RecvError::Lagged`] (with the count missed) if this subscriber
    /// fell behind and the channel overwrote events it had not yet read; the
    /// next call then resumes from the oldest still-buffered event. Returns
    /// [`RecvError::Closed`] once every [`EventStream`] handle has been dropped.
    ///
    /// # Errors
    ///
    /// Propagates the [`RecvError`] from the underlying broadcast receiver
    /// (`Lagged` on drop-oldest overflow, `Closed` when the publisher is gone).
    pub async fn recv(&mut self) -> Result<SeqEvent<T>, RecvError> {
        self.rx.recv().await
    }

    /// Try to receive the next buffered event without awaiting.
    ///
    /// # Errors
    ///
    /// Returns [`TryRecvError::Empty`] when nothing is buffered,
    /// [`TryRecvError::Lagged`] when this subscriber missed drop-oldest events,
    /// or [`TryRecvError::Closed`] when the publisher is gone.
    pub fn try_recv(&mut self) -> Result<SeqEvent<T>, TryRecvError> {
        self.rx.try_recv()
    }

    /// The number of events buffered for this subscriber that have not yet been
    /// received. Bounded by the channel capacity; reading it takes no lock the
    /// publisher needs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rx.len()
    }

    /// Whether this subscriber currently has no buffered events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }

    /// Re-subscribe from the current head, discarding this subscriber's buffered
    /// backlog (used to deliberately resynchronize after a [`RecvError::Lagged`]).
    #[must_use]
    pub fn resubscribe(&self) -> Self {
        Self {
            rx: self.rx.resubscribe(),
        }
    }
}

/// The engine's outbound publish handle, bundling the wait-free latest-state
/// slot and the drop-oldest event stream.
///
/// The runtime holds one of these and calls [`EnginePublisher::publish_state`]
/// and [`EnginePublisher::publish_event`] once per tick. Both publish paths are
/// physically incapable of being blocked
/// by a consumer (invariant #10): the state slot is a wait-free atomic store and
/// the event stream is a `broadcast::Sender::send` that never blocks on slow
/// receivers.
#[derive(Debug)]
pub struct EnginePublisher<S, E> {
    /// The latest engine-state snapshot (newest-wins).
    pub state: LatestState<S>,
    /// The drop-oldest event stream (with resume-by-seq).
    pub events: EventStream<E>,
}

impl<S, E> EnginePublisher<S, E> {
    /// Build a publisher with an empty state slot and an event stream of the
    /// given per-subscriber `event_capacity` (drop-oldest depth).
    #[must_use]
    pub fn new(event_capacity: usize) -> Self {
        let (events, initial) = event_stream(event_capacity);
        // Drop the initial subscription: the engine publishes unconditionally and
        // never requires a live subscriber, so we do not retain one.
        drop(initial);
        Self {
            state: LatestState::new(),
            events,
        }
    }

    /// Publish a state snapshot. Wait-free; returns its sequence number.
    pub fn publish_state(&self, snapshot: S) -> u64 {
        self.state.publish(snapshot)
    }

    /// Publish an event. Non-blocking drop-oldest; returns its sequence number.
    pub fn publish_event(&self, event: E) -> u64 {
        self.events.publish(event)
    }

    /// Subscribe to the event stream (events published after this call).
    #[must_use]
    pub fn subscribe(&self) -> EventSubscription<E> {
        self.events.subscribe()
    }
}

impl<S, E> Clone for EnginePublisher<S, E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            events: self.events.clone(),
        }
    }
}
