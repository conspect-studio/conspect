//! The compressed tap (output-routing §12.5): the ingest side's bounded hand-off
//! of a source's coded access units to every passthrough clock reading it.
//!
//! ## Shape
//!
//! One [`CompressedTap`] per source. The demuxer (ingest thread) is the only
//! **writer** ([`CompressedTap::offer`]); each passthrough clock that reads the
//! source holds one [`TapReader`] from [`CompressedTap::subscribe`]. A std
//! `mpsc` channel has exactly one receiver, so the tap fans out: every consumer
//! owns its own bounded `sync_channel` of [`TAP_CAPACITY`] AUs, and the writer
//! holds the matching senders in a small list (at most [`MAX_TAP_CONSUMERS`]).
//!
//! ## Never blocking (invariants #1 / #10)
//!
//! * The writer only ever `try_send`s. A full consumer channel drops the AU
//!   **for that consumer only** and counts it ([`CompressedTap::dropped`]);
//!   the writer never waits for a reader.
//! * The clock side only ever `try_recv`s ([`TapReader::try_recv`]); it never
//!   takes the sender-list mutex. That mutex is taken only by the writer
//!   (`offer`) and by `subscribe` / reader drop (control-plane events), so a
//!   clock thread can never contend on it.
//! * With no consumer, [`CompressedTap::has_consumers`] is a single atomic load
//!   and [`CompressedTap::offer`] returns before touching anything, so the
//!   ingest side can skip even building the [`TapAu`] (copying the bytes).
//!
//! ## Drop policy (recorded deviation from "drop-oldest")
//!
//! A per-consumer channel drops the **newest** AU when its 64 slots are unread.
//! A live clock drains its reader every tick (up to the same bound), so 64
//! unread AUs means that clock is dead or wedged; dropping the newest there is
//! harmless. The drop-**oldest** bound the design names is the clock's jitter
//! buffer (8 ticks, [`super::clock`]), which is where real jitter is absorbed.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use multiview_core::time::Rational;

/// Capacity of each consumer's channel, in access units.
pub const TAP_CAPACITY: usize = 64;

/// Maximum number of simultaneous consumers of one source's tap.
pub const MAX_TAP_CONSUMERS: usize = 8;

/// One coded access unit as the ingest side hands it to passthrough.
///
/// Classification arrives as plain fields (this crate does not depend on
/// `multiview-ffmpeg` in the default build): the ingest side computes
/// `strict_idr` with `idr::is_self_contained_idr` and `reference` from the NAL
/// header (`nal_ref_idc != 0`).
#[derive(Debug, Clone)]
pub struct TapAu {
    /// The access unit bytes, shared (one allocation for every consumer).
    pub data: Arc<[u8]>,
    /// Source decode timestamp in `time_base` units.
    pub dts: i64,
    /// Source presentation timestamp in `time_base` units.
    pub pts: i64,
    /// The source stream's time base (seconds per unit).
    pub time_base: Rational,
    /// A strict random-access point: an IDR carrying its parameter sets in band.
    pub strict_idr: bool,
    /// Whether the AU is a reference picture (`nal_ref_idc != 0` for H.264).
    pub reference: bool,
    /// Per-source parameter-set generation; the ingest side bumps it whenever
    /// the in-band parameter sets change.
    pub params_gen: u64,
    /// Arrival time on the shared monotonic clock, in nanoseconds.
    pub arrival_ns: u64,
}

/// One registered consumer: its id and the sending half of its channel.
struct Consumer {
    id: u64,
    tx: SyncSender<TapAu>,
}

/// State shared between the tap and its readers.
struct TapShared {
    /// Number of live readers (an atomic so the writer's fast path is lock-free).
    consumers: AtomicUsize,
    /// The per-consumer senders. Taken by the writer and (un)subscribe only.
    senders: Mutex<Vec<Consumer>>,
    /// Monotonic id source for consumers.
    next_id: AtomicU64,
    /// AUs dropped because a consumer's channel was full.
    dropped: AtomicU64,
}

impl TapShared {
    fn lock(&self) -> MutexGuard<'_, Vec<Consumer>> {
        // A poisoned lock only means another thread panicked while holding it;
        // the Vec itself stays structurally valid, so keep going.
        self.senders.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The per-source compressed tap (writer side). Cheap to clone; clones share
/// one consumer list.
#[derive(Clone)]
pub struct CompressedTap {
    shared: Arc<TapShared>,
}

impl Default for CompressedTap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CompressedTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompressedTap")
            .field("consumers", &self.consumer_count())
            .field("dropped", &self.dropped())
            .finish()
    }
}

impl CompressedTap {
    /// A tap with no consumers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(TapShared {
                consumers: AtomicUsize::new(0),
                senders: Mutex::new(Vec::with_capacity(MAX_TAP_CONSUMERS)),
                next_id: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            }),
        }
    }

    /// Whether any clock is reading this tap. One atomic load; the ingest side
    /// checks this before copying an AU's bytes.
    #[must_use]
    pub fn has_consumers(&self) -> bool {
        self.shared.consumers.load(Ordering::Acquire) > 0
    }

    /// Number of live readers.
    #[must_use]
    pub fn consumer_count(&self) -> usize {
        self.shared.consumers.load(Ordering::Acquire)
    }

    /// AUs dropped because a consumer's channel was full (summed over consumers).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Offer one AU to every consumer. Never blocks: with no consumer it returns
    /// at once; a full consumer channel drops this AU for that consumer and
    /// counts it.
    pub fn offer(&self, au: TapAu) {
        if !self.has_consumers() {
            return;
        }
        let senders = self.shared.lock();
        let Some((last, rest)) = senders.split_last() else {
            return;
        };
        for consumer in rest {
            self.send_one(&consumer.tx, au.clone());
        }
        self.send_one(&last.tx, au);
    }

    fn send_one(&self, tx: &SyncSender<TapAu>, au: TapAu) {
        // A disconnected reader is being dropped; its guard removes the sender.
        if let Err(TrySendError::Full(_)) = tx.try_send(au) {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Register a new consumer. Returns `None` when [`MAX_TAP_CONSUMERS`]
    /// readers already exist.
    #[must_use]
    pub fn subscribe(&self) -> Option<TapReader> {
        let mut senders = self.shared.lock();
        if senders.len() >= MAX_TAP_CONSUMERS {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(TAP_CAPACITY);
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        senders.push(Consumer { id, tx });
        self.shared.consumers.fetch_add(1, Ordering::AcqRel);
        Some(TapReader {
            rx,
            id,
            shared: Arc::clone(&self.shared),
        })
    }
}

/// One consumer's read handle. Dropping it unsubscribes.
pub struct TapReader {
    rx: Receiver<TapAu>,
    id: u64,
    shared: Arc<TapShared>,
}

impl std::fmt::Debug for TapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapReader")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl TapReader {
    /// The next queued AU, if any. Never blocks.
    #[must_use]
    pub fn try_recv(&self) -> Option<TapAu> {
        self.rx.try_recv().ok()
    }

    /// A non-blocking iterator over whatever is queued now (`try_recv` until
    /// empty). The clock bounds how many it takes per tick.
    pub fn drain(&self) -> impl Iterator<Item = TapAu> + '_ {
        std::iter::from_fn(move || self.try_recv())
    }
}

impl Drop for TapReader {
    fn drop(&mut self) {
        let mut senders = self.shared.lock();
        let before = senders.len();
        senders.retain(|c| c.id != self.id);
        if senders.len() != before {
            self.shared.consumers.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(n: i64) -> TapAu {
        TapAu {
            data: Arc::from(vec![0_u8, 0, 1, 9]),
            dts: n,
            pts: n,
            time_base: Rational::new(1, 90_000),
            strict_idr: false,
            reference: true,
            params_gen: 1,
            arrival_ns: 0,
        }
    }

    #[test]
    fn no_consumer_queues_nothing() {
        let tap = CompressedTap::new();
        assert!(!tap.has_consumers());
        for n in 0..1000 {
            tap.offer(au(n));
        }
        assert_eq!(
            tap.dropped(),
            0,
            "no consumer: nothing built, nothing dropped"
        );
        // A late subscriber sees nothing that was offered before it existed.
        let reader = tap.subscribe().unwrap();
        assert!(reader.try_recv().is_none());
    }

    #[test]
    fn full_channel_drops_and_counts_without_blocking() {
        let tap = CompressedTap::new();
        let reader = tap.subscribe().unwrap();
        let extra = 100_u64;
        let total = u64::try_from(TAP_CAPACITY).unwrap() + extra;
        for n in 0..total {
            // Never blocks even though nobody reads.
            tap.offer(au(i64::try_from(n).unwrap()));
        }
        assert_eq!(tap.dropped(), extra);
        // The channel holds the first TAP_CAPACITY (drop-newest).
        let got: Vec<i64> = reader.drain().map(|a| a.dts).collect();
        assert_eq!(got.len(), TAP_CAPACITY);
        assert_eq!(got.first(), Some(&0));
        assert_eq!(
            got.last(),
            Some(&(i64::try_from(TAP_CAPACITY).unwrap() - 1))
        );
    }

    #[test]
    fn a_full_consumer_does_not_starve_another() {
        let tap = CompressedTap::new();
        let slow = tap.subscribe().unwrap();
        let fast = tap.subscribe().unwrap();
        let mut fast_seen = 0_usize;
        for n in 0..500 {
            tap.offer(au(n));
            fast_seen += fast.drain().count();
        }
        assert_eq!(fast_seen, 500, "the draining consumer gets every AU");
        assert_eq!(slow.drain().count(), TAP_CAPACITY);
        assert_eq!(tap.dropped(), 500 - u64::try_from(TAP_CAPACITY).unwrap());
    }

    #[test]
    fn subscribe_is_bounded_and_drop_unsubscribes() {
        let tap = CompressedTap::new();
        let readers: Vec<TapReader> = (0..MAX_TAP_CONSUMERS)
            .map(|_| tap.subscribe().unwrap())
            .collect();
        assert_eq!(tap.consumer_count(), MAX_TAP_CONSUMERS);
        assert!(tap.subscribe().is_none(), "consumer list is bounded");
        drop(readers);
        assert_eq!(tap.consumer_count(), 0);
        assert!(!tap.has_consumers());
        tap.offer(au(0));
        assert_eq!(tap.dropped(), 0);
        assert!(tap.subscribe().is_some());
    }

    #[test]
    fn shares_one_allocation_across_consumers() {
        let tap = CompressedTap::new();
        let a = tap.subscribe().unwrap();
        let b = tap.subscribe().unwrap();
        let x = au(7);
        let ptr = Arc::as_ptr(&x.data);
        tap.offer(x);
        assert_eq!(Arc::as_ptr(&a.try_recv().unwrap().data), ptr);
        assert_eq!(Arc::as_ptr(&b.try_recv().unwrap().data), ptr);
    }
}
