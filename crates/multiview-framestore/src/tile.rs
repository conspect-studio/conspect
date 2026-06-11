//! The per-tile last-good-frame store: lock-free slot + failure-ladder state.
//!
//! [`TileStore<T>`] ties together the lock-free [`LatestSlot`] (the
//! last-good-frame cell) and the [`classify`] failure-ladder policy. It is the
//! concrete realization of invariant #2 for a single tile:
//!
//! * **Writers** (a decoder) call [`TileStore::publish`] with the frame and the
//!   timeline instant it arrived. This is non-blocking and newest-wins.
//! * **Readers** (the compositor, on each output tick) call [`TileStore::read`]
//!   with the current instant. They never block and always get a definite
//!   answer: a fresh frame, the *held* last-good frame, or an explicit
//!   `NoSignal` indicator — never a stall.
//!
//! Time is **injected**: every method that needs "now" takes a [`MediaTime`],
//! so the whole state ladder is deterministically testable with no real clock
//! and no sleeps.
use core::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;

use crate::latest::LatestSlot;
use crate::state::{classify, TileThresholds};

/// Select the entry whose `at` is nearest-but-not-after `now`, falling back to
/// the earliest retained entry when every entry is still ahead of `now`.
/// Returns `None` only for an empty ring. The ring is ascending in `at`, so this
/// is a binary search for the rightmost entry `<= now`.
fn select_nearest_not_after<T>(ring: &[RingEntry<T>], now: MediaTime) -> Option<&RingEntry<T>> {
    let first = ring.first()?;
    // Partition point: count of entries with `at <= now`. The last such entry is
    // the latch; if there are none, every entry is ahead of `now` → earliest.
    let idx = ring.partition_point(|e| e.at <= now);
    if idx == 0 {
        Some(first)
    } else {
        ring.get(idx.saturating_sub(1)).or(Some(first))
    }
}

/// The latched read of a ring at `now`: the entry nearest-but-not-after `now`
/// (see [`select_nearest_not_after`]) paired with the failure-ladder state of
/// that **latched** entry — its lag behind output time (`now - entry.at`)
/// classified against `thresholds`.
///
/// This is the single source of truth shared by
/// [`read_at`](TileStore::read_at) and [`state_at`](TileStore::state_at) so the
/// two can never drift: a tile's reported state always describes the frame the
/// compositor actually draws, not the producer's newest decode.
fn latch_and_classify<T>(
    ring: &[RingEntry<T>],
    now: MediaTime,
    thresholds: TileThresholds,
) -> Option<(&RingEntry<T>, SourceState)> {
    let selected = select_nearest_not_after(ring, now)?;
    // The latched frame's lag behind output time drives the failure ladder: a
    // frame at-or-after `now` is fresh (lag 0); a frame falling behind (the
    // producer stalled / the clip ended) ages exactly as the ladder expects.
    let lag = now.saturating_sub(selected.at);
    Some((selected, classify(lag, thresholds)))
}

/// One retained frame in the media-time ring: the source-relative media instant
/// it represents, paired with the frame. Ordered by `at` (publishes are
/// monotonic in media time on the normal path; an out-of-order or backwards
/// stamp re-anchors the ring, see [`TileStore::publish_arc`]).
#[derive(Debug)]
struct RingEntry<T> {
    at: MediaTime,
    frame: Arc<T>,
}

impl<T> Clone for RingEntry<T> {
    fn clone(&self) -> Self {
        Self {
            at: self.at,
            frame: Arc::clone(&self.frame),
        }
    }
}

/// Sentinel stored in [`TileStore::last_frame_at_ns`] meaning "no frame has
/// ever been published". `i64::MIN` can never be a real publish instant
/// (timelines are non-negative and bounded well away from the `i64` extremes),
/// so it unambiguously encodes the not-yet-published state without a separate
/// flag or an `Option` allocation.
const NEVER_PUBLISHED: i64 = i64::MIN;

/// The outcome of reading a tile on an output tick.
///
/// A reader always receives one of these — the compositor never blocks waiting
/// for a tile (invariant #2 / #1). `#[non_exhaustive]` so new render hints can
/// be added without breaking downstream `match`es.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TileRead<T> {
    /// A fresh frame within the `hold` window — composite it directly.
    ///
    /// Carries the tile's current [`SourceState`] (always [`SourceState::Live`]
    /// for this variant) for uniform telemetry.
    Fresh {
        /// The frame to composite.
        frame: Arc<T>,
    },
    /// No fresh frame, but a last-good frame is held — composite it (the tile
    /// is `STALE` or `RECONNECTING`), letting the rest of the canvas continue.
    Held {
        /// The held last-good frame.
        frame: Arc<T>,
        /// The degraded state the tile is now in (`Stale`/`Reconnecting`, or
        /// `NoSignal` if a frame was ever seen but the no-signal threshold has
        /// elapsed).
        state: SourceState,
    },
    /// No usable signal: the tile has either never received a frame, or has
    /// been starved past the `nosignal` threshold and the policy is to stop
    /// holding. The compositor should render a placeholder/slate card.
    NoSignal,
}

impl<T> TileRead<T> {
    /// The [`SourceState`] this read corresponds to.
    #[must_use]
    pub fn state(&self) -> SourceState {
        match self {
            Self::Fresh { .. } => SourceState::Live,
            Self::Held { state, .. } => *state,
            Self::NoSignal => SourceState::NoSignal,
        }
    }

    /// The held/fresh frame, if any (`None` for [`TileRead::NoSignal`]).
    #[must_use]
    pub fn frame(&self) -> Option<&Arc<T>> {
        match self {
            Self::Fresh { frame } | Self::Held { frame, .. } => Some(frame),
            Self::NoSignal => None,
        }
    }
}

/// Whether a tile should keep holding its last-good frame once it crosses the
/// `nosignal` threshold, or hand back a `NoSignal` indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum NoSignalPolicy {
    /// Stop holding once `NO_SIGNAL` is reached: [`TileStore::read`] returns
    /// [`TileRead::NoSignal`] so the compositor draws a slate card. This is the
    /// broadcast-correct default (a frozen frame for minutes is misleading).
    #[default]
    Slate,
    /// Keep holding the last-good frame even in `NO_SIGNAL` (still reports the
    /// `NoSignal` *state* via [`TileRead::Held`]). Useful for tiles where a
    /// frozen last frame is preferable to a slate.
    HoldForever,
}

/// A per-tile last-good-frame store with an attached failure-ladder state
/// machine.
///
/// Generic over the stored payload `T` (a backend frame handle, a decoded
/// surface wrapper, …). The store itself only ever holds `Arc<T>`, so cloning a
/// read is cheap and tear-free.
#[derive(Debug)]
pub struct TileStore<T> {
    id: Arc<str>,
    slot: LatestSlot<T>,
    thresholds: TileThresholds,
    policy: NoSignalPolicy,
    /// Timeline instant of the most recent published frame, as raw nanoseconds,
    /// or [`NEVER_PUBLISHED`] until the first frame arrives. A plain atomic
    /// (not an `Arc` cell) so a reader observes it lock-free with no allocation
    /// and no extra reclamation machinery alongside the frame slot.
    last_frame_at_ns: AtomicI64,
    /// A bounded, media-time-ordered ring of recently-published frames, used by
    /// [`read_at`](TileStore::read_at) to latch the frame nearest-but-not-after
    /// the output clock's instant (streaming-gotchas §1). Stored as a snapshot
    /// behind an [`ArcSwap`] so the reader is **lock-free and never blocks** on a
    /// writer (invariant #1): the writer copies the (≤ [`RING_CAPACITY`]) ring,
    /// appends, drops the oldest beyond capacity, and atomically swaps it in.
    ///
    /// The publish is O(capacity) copy-on-write **by design** (ADR-T009): that is
    /// what gives the binary-search read a stable, fully-ordered, torn-read-free
    /// snapshot. An in-place O(1) ring would either move O(capacity) onto this hot
    /// read path or require a seqlock that adds retry to it — the wrong trade,
    /// since the copy runs on the *sampled input thread* (never the output clock).
    ///
    /// [`RING_CAPACITY`]: TileStore::RING_CAPACITY
    ring: ArcSwap<Vec<RingEntry<T>>>,
}

impl<T> TileStore<T> {
    /// Capacity of the bounded media-time ring used by
    /// [`read_at`](TileStore::read_at). Sized to absorb a producer running well
    /// ahead of a temporarily-slow output loop (drop-oldest beyond this), while
    /// staying small and bounded (invariant: queues drop, never grow).
    pub const RING_CAPACITY: usize = 256;

    /// Create an empty tile store with the given id, thresholds, and no-signal
    /// policy. Until the first [`publish`](TileStore::publish), the tile is in
    /// [`SourceState::NoSignal`].
    #[must_use]
    pub fn new(
        id: impl Into<Arc<str>>,
        thresholds: TileThresholds,
        policy: NoSignalPolicy,
    ) -> Self {
        Self {
            id: id.into(),
            slot: LatestSlot::new(),
            thresholds,
            policy,
            last_frame_at_ns: AtomicI64::new(NEVER_PUBLISHED),
            ring: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Create a tile store with default thresholds and the [`NoSignalPolicy::Slate`]
    /// default.
    #[must_use]
    pub fn with_defaults(id: impl Into<Arc<str>>) -> Self {
        Self::new(id, TileThresholds::default(), NoSignalPolicy::default())
    }

    /// The stable tile/source identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The configured thresholds.
    #[must_use]
    pub fn thresholds(&self) -> TileThresholds {
        self.thresholds
    }

    /// Publish a fresh frame that arrived at timeline instant `at`.
    ///
    /// Non-blocking, newest-wins: any previously held frame is dropped. Records
    /// `at` as the last-fresh-frame instant, which resets the failure ladder to
    /// [`SourceState::Live`] (as observed by a subsequent [`read`](TileStore::read)
    /// at `now >= at` with `now - at < hold`).
    ///
    /// Returns the slot sequence number assigned to this frame.
    pub fn publish(&self, frame: T, at: MediaTime) -> u64 {
        self.publish_arc(Arc::new(frame), at)
    }

    /// As [`publish`](TileStore::publish), for a frame already wrapped in an
    /// [`Arc`].
    pub fn publish_arc(&self, frame: Arc<T>, at: MediaTime) -> u64 {
        let seq = self.slot.publish_arc(Arc::clone(&frame));
        // Append to the media-time ring (drop-oldest, bounded — invariant: queues
        // drop, never grow). The ring backs the latch-on-tick `read_at`. A frame
        // whose stamp goes backwards (a discontinuity / new source generation
        // after reconnect) re-anchors the ring on itself rather than leaving a
        // stale future-stamped frame ahead of it, so sampling cannot get stuck
        // pointing past the new content.
        let entry = RingEntry { at, frame };
        self.ring.rcu(|current| {
            let mut next: Vec<RingEntry<T>> = if current.last().is_some_and(|tail| at < tail.at) {
                // Backwards stamp: discard the now-superseded future and restart.
                Vec::with_capacity(1)
            } else {
                Vec::clone(current)
            };
            next.push(entry.clone());
            // Drop the oldest entries beyond the bounded capacity.
            let overflow = next.len().saturating_sub(Self::RING_CAPACITY);
            if overflow > 0 {
                next.drain(0..overflow);
            }
            next
        });
        // Record the arrival instant after the frame is visible in the slot, so
        // a reader that observes a fresh `last_frame_at_ns` is guaranteed to
        // also see (at least) this frame in the slot — never a newer timestamp
        // with an older/missing frame.
        self.last_frame_at_ns
            .store(at.as_nanos(), Ordering::Release);
        seq
    }

    /// The elapsed time since the last fresh frame as of `now`, or [`None`] if
    /// no frame has ever been published.
    ///
    /// Saturating: a `now` earlier than the last frame yields `0` (the
    /// monotonic-guard case), never a negative duration.
    #[must_use]
    pub fn elapsed_since_frame(&self, now: MediaTime) -> Option<MediaTime> {
        let last_ns = self.last_frame_at_ns.load(Ordering::Acquire);
        if last_ns == NEVER_PUBLISHED {
            return None;
        }
        Some(now.saturating_sub(MediaTime::from_nanos(last_ns)))
    }

    /// Whether at least one frame has ever been published into this tile.
    ///
    /// A cheap, lock-free, wait-free read of the last-frame atomic — no ring
    /// snapshot, no allocation. `false` until the first
    /// [`publish`](TileStore::publish)/[`publish_arc`](TileStore::publish_arc),
    /// `true` forever after (the store is newest-wins, never cleared back to
    /// empty). The startup *prime-wait* uses this to tell a tile that has decoded
    /// its first frame from one that is still cold, so the output clock can hold
    /// its very first tick until inputs are primed-or-timed-out (without ever
    /// blocking on a dead source — the caller bounds the wait).
    #[must_use]
    pub fn is_primed(&self) -> bool {
        self.last_frame_at_ns.load(Ordering::Acquire) != NEVER_PUBLISHED
    }

    /// The tile's **producer-liveness** [`SourceState`] as of `now`.
    ///
    /// Pure function of `now`, the *newest* published frame's instant, and the
    /// thresholds. A tile that has never received a frame is
    /// [`SourceState::NoSignal`].
    ///
    /// This measures how long since the producer *last published*, regardless of
    /// which frame the output clock is latched onto. It is the right signal for
    /// **input/decoder health** (is the source still delivering?), but it is
    /// **not** the state of the picture on screen: a producer decoding ahead can
    /// publish a frame whose source-media stamp sits in the future relative to
    /// `now`, which makes this report [`SourceState::Live`] while the *latched*
    /// frame the compositor actually draws has aged to `NO_SIGNAL`. For the
    /// state of the drawn picture — the one the compositor and telemetry should
    /// surface — use [`state_at`](TileStore::state_at) / [`read_at`](TileStore::read_at).
    #[must_use]
    pub fn state(&self, now: MediaTime) -> SourceState {
        match self.elapsed_since_frame(now) {
            Some(elapsed) => classify(elapsed, self.thresholds),
            None => SourceState::NoSignal,
        }
    }

    /// The tile's [`SourceState`] for the frame the output clock is **latched
    /// onto** at output media time `now` (streaming-gotchas §1).
    ///
    /// Classifies on the *latched* frame's lag behind output time
    /// (`now - latched.at`), the identical rule [`read_at`](TileStore::read_at)
    /// applies to the frame it returns — they share one helper so the reported
    /// state always describes the picture actually drawn. A tile that has never
    /// received a frame (empty ring) is [`SourceState::NoSignal`].
    ///
    /// Prefer this over [`state`](TileStore::state) for telemetry and the
    /// degradation signal: an ahead-decoding source whose newest frame is
    /// future-stamped reports `LIVE` from [`state`](TileStore::state) (producer
    /// liveness) even as its on-screen picture freezes and ages — `state_at`
    /// follows the on-screen picture, so a frozen/exhausted source correctly
    /// rides `LIVE` → `STALE` → `NO_SIGNAL` as output time runs past its last
    /// frame.
    #[must_use]
    pub fn state_at(&self, now: MediaTime) -> SourceState {
        let ring = self.ring.load();
        latch_and_classify(&ring, now, self.thresholds)
            .map_or(SourceState::NoSignal, |(_, state)| state)
    }

    /// Read the tile on an output tick at instant `now`.
    ///
    /// Never blocks. Returns:
    /// * [`TileRead::Fresh`] when a frame is held and the tile is `LIVE`;
    /// * [`TileRead::Held`] when a frame is held but the tile is degraded
    ///   (`STALE`/`RECONNECTING`, or `NO_SIGNAL` under
    ///   [`NoSignalPolicy::HoldForever`]);
    /// * [`TileRead::NoSignal`] when no frame is held, or the tile is
    ///   `NO_SIGNAL` under [`NoSignalPolicy::Slate`].
    #[must_use]
    pub fn read(&self, now: MediaTime) -> TileRead<T> {
        let Some(frame) = self.slot.load() else {
            // Nothing ever published (or explicitly cleared).
            return TileRead::NoSignal;
        };
        match self.state(now) {
            SourceState::Live => TileRead::Fresh { frame },
            state @ (SourceState::Stale | SourceState::Reconnecting) => {
                TileRead::Held { frame, state }
            }
            SourceState::NoSignal => match self.policy {
                NoSignalPolicy::HoldForever => TileRead::Held {
                    frame,
                    state: SourceState::NoSignal,
                },
                NoSignalPolicy::Slate => TileRead::NoSignal,
            },
            // `SourceState` is `#[non_exhaustive]`; treat any future state
            // conservatively as "no usable fresh signal".
            _ => match self.policy {
                NoSignalPolicy::HoldForever => TileRead::Held {
                    frame,
                    state: self.state(now),
                },
                NoSignalPolicy::Slate => TileRead::NoSignal,
            },
        }
    }

    /// Read the tile latched to **output media time** `now` (streaming-gotchas
    /// §1, "latch-on-tick"): selects the retained frame whose source-relative
    /// `media_time` is *nearest-but-not-after* `now`, so the tile advances exactly
    /// in step with the output clock regardless of how fast the producer decoded
    /// (a producer that ran far ahead does not race the tile; a stalled producer
    /// holds; a finite source past its end freezes on its last frame).
    ///
    /// Selection rule:
    /// * the latest entry with `entry.at <= now` (the latched frame), else
    /// * the earliest *retained* entry (the first frame's stamp may sit a touch
    ///   after the very first tick, or old entries were evicted — show content,
    ///   not a slate), else
    /// * [`TileRead::NoSignal`] when nothing has ever been published.
    ///
    /// The freshness ladder is evaluated on the *latched* frame's lag behind
    /// `now` (`now - entry.at`), so a frozen / exhausted source correctly ages
    /// into `STALE` → `NO_SIGNAL` as output time runs on past its last frame,
    /// while a source tracking output time stays `LIVE`.
    ///
    /// Never blocks: it reads an atomic snapshot of the bounded ring.
    #[must_use]
    pub fn read_at(&self, now: MediaTime) -> TileRead<T> {
        let ring = self.ring.load();
        let Some((selected, state)) = latch_and_classify(&ring, now, self.thresholds) else {
            return TileRead::NoSignal;
        };
        let frame = Arc::clone(&selected.frame);
        match state {
            SourceState::Live => TileRead::Fresh { frame },
            SourceState::Stale | SourceState::Reconnecting => TileRead::Held { frame, state },
            SourceState::NoSignal => match self.policy {
                NoSignalPolicy::HoldForever => TileRead::Held {
                    frame,
                    state: SourceState::NoSignal,
                },
                NoSignalPolicy::Slate => TileRead::NoSignal,
            },
            // `SourceState` is `#[non_exhaustive]`; treat any future state
            // conservatively (mirror `read`).
            _ => match self.policy {
                NoSignalPolicy::HoldForever => TileRead::Held { frame, state },
                NoSignalPolicy::Slate => TileRead::NoSignal,
            },
        }
    }

    /// The most recent slot sequence number (`0` if nothing published).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.slot.sequence()
    }

    /// Borrow the underlying lock-free slot (e.g. to share a reader handle).
    #[must_use]
    pub fn slot(&self) -> &LatestSlot<T> {
        &self.slot
    }
}
