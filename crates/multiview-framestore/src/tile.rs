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
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
/// it represents and the globally-monotonic publish `seq` it was stamped with,
/// paired with the frame. Ordered by `at` for selection (publishes are monotonic
/// in media time on the normal path; an out-of-order or backwards stamp
/// re-anchors the ring, see [`TileStore::publish_arc`]); `seq` gives a total,
/// never-reset order across source generations that the prune watermark keys on.
#[derive(Debug)]
struct RingEntry<T> {
    /// The source-relative media instant this frame represents — the selection
    /// key for [`select_nearest_not_after`].
    at: MediaTime,
    /// The tile-global publish sequence (from [`LatestSlot`]): strictly
    /// increasing across every publish and never reset, so a newer source
    /// generation always outranks an older one. The prune watermark is keyed on
    /// `seq`, not `at`, precisely so a reader holding a superseded
    /// (old-generation) snapshot can never advance it past a newer generation's
    /// frames.
    seq: u64,
    frame: Arc<T>,
}

impl<T> Clone for RingEntry<T> {
    fn clone(&self) -> Self {
        Self {
            at: self.at,
            seq: self.seq,
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

/// Watermark value meaning "no reader has latched a ring frame yet". `0` is below
/// every real publish sequence (the first publish is `seq` `1`; see
/// [`LatestSlot::publish_arc`]), so `retain(|e| e.seq >= NO_LATCH_SEQ)` keeps the
/// whole ring until a reader has actually selected a frame — nothing is pruned
/// before it has been consumed.
const NO_LATCH_SEQ: u64 = 0;

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
    /// The **publish sequence** of the frame the output clock last **latched**
    /// onto (via [`read_at`](TileStore::read_at) / [`state_at`](TileStore::state_at)),
    /// or [`NO_LATCH_SEQ`] until the first latch. [`publish_arc`] prunes every
    /// ring entry whose `seq` is *strictly below* this watermark: those frames
    /// sit before the frame the compositor is showing and — output `now` being
    /// monotonic (invariant #1/#3) — can never be selected again, so dropping
    /// them collapses the ring to the latched frame plus any decode-ahead future
    /// instead of a full [`RING_CAPACITY`] backlog of dead frames (invariant #9).
    ///
    /// **Keyed on `seq`, not media time, for cross-generation safety.** The
    /// sequence is tile-global and never reset, so a newer source generation
    /// (after a reconnect re-anchors the ring to a lower media time) always
    /// carries strictly higher `seq`s than any frame an older snapshot holds. A
    /// reader still holding a superseded snapshot can therefore only ever
    /// `fetch_max` this watermark to an *old, lower* sequence — one already below
    /// the new generation's frames — so it can never make the next publish prune
    /// the reconnected generation (the cross-generation race a media-time
    /// watermark admitted). Advanced with a monotonic `fetch_max`: it never
    /// regresses, and `read_at`/`state_at` running on the single output-clock
    /// thread only push it forward.
    ///
    /// [`RING_CAPACITY`]: TileStore::RING_CAPACITY
    /// [`publish_arc`]: TileStore::publish_arc
    latch_watermark_seq: AtomicU64,
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
            latch_watermark_seq: AtomicU64::new(NO_LATCH_SEQ),
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
        //
        // Publishes are single-writer per tile (one actor per source), so the
        // tail observed here is stable against other writers; only the lock-free
        // readers touch the ring concurrently, and they never mutate it.
        //
        // Frames the output clock has already advanced past are dead weight
        // (invariant #9): output `now` is monotone, so nothing whose publish `seq`
        // is strictly below the latch watermark is ever selected again. Pruning
        // them holds the ring at the latched frame + any decode-ahead future
        // rather than a full RING_CAPACITY backlog. This runs inside the existing
        // O(capacity) copy-on-write on the sampled input thread — never on the
        // output clock.
        //
        // The watermark is a publish `seq`, not a media time, so a reconnect
        // needs no special-casing here: the re-anchored generation's frames carry
        // strictly higher `seq`s than anything an old snapshot holds, so a stale
        // cross-generation `fetch_max` can only leave the watermark *below* them
        // and the `retain` keeps them (see the `latch_watermark_seq` field).
        let watermark_seq = self.latch_watermark_seq.load(Ordering::Relaxed);
        let entry = RingEntry { at, seq, frame };
        self.ring.rcu(|current| {
            let mut next: Vec<RingEntry<T>> = if current.last().is_some_and(|tail| at < tail.at) {
                // Backwards stamp: discard the now-superseded future and restart
                // the ring on the re-anchored frame, so selection cannot stay
                // stuck pointing past the fresh content.
                Vec::with_capacity(1)
            } else {
                let mut kept = Vec::clone(current);
                // Keep every entry whose `seq` is at-or-after the watermark; drop
                // ONLY those strictly before it. The latched frame sits exactly at
                // the watermark (a reader `fetch_max`'d it to its own `seq`), so
                // `>=` retains it — the boundary frame the compositor is still
                // showing is never pruned, only strictly-consumed past frames.
                kept.retain(|e| e.seq >= watermark_seq);
                kept
            };
            next.push(entry.clone());
            // Hard ceiling (drop-oldest) bounds the worst case: a stalled output
            // clock that stops advancing the watermark while the producer runs on.
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

    /// Latch onto the ring frame nearest-but-not-after `now` and advance the
    /// prune watermark to that frame's publish `seq`. **The single point at which
    /// any reader selects a frame from the ring** — [`read_at`](TileStore::read_at)
    /// and [`state_at`](TileStore::state_at) both route through it, and any future
    /// reader (e.g. a wired-up degradation sampler) MUST too.
    ///
    /// Fusing selection with the watermark advance is what makes
    /// [`publish_arc`](TileStore::publish_arc)'s prune correct *by construction*:
    /// the producer drops only entries whose `seq` is **strictly below** the
    /// watermark, so the frame this method selects — whose `seq` is written into
    /// the watermark by the `fetch_max` before it is returned — can never be
    /// pruned out from under the caller. A reader physically cannot obtain a ring
    /// frame without first protecting it.
    ///
    /// Keying on `seq` (a tile-global, never-reset order) rather than media time
    /// is what makes this safe across a reconnect: a reader holding a superseded
    /// snapshot advances the watermark only to an *old* `seq`, always below the
    /// re-anchored generation's frames, so it can never prune the new generation
    /// (see the `latch_watermark_seq` field). `fetch_max` is monotone: the
    /// watermark only ever moves forward and never regresses.
    fn latch_and_advance<'r>(
        &self,
        ring: &'r [RingEntry<T>],
        now: MediaTime,
    ) -> Option<(&'r RingEntry<T>, SourceState)> {
        let (selected, state) = latch_and_classify(ring, now, self.thresholds)?;
        self.latch_watermark_seq
            .fetch_max(selected.seq, Ordering::Relaxed);
        Some((selected, state))
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
        // Route through `latch_and_advance` so a monitored-but-not-composited
        // tile (state sampled, never `read_at`) still advances the watermark and
        // bounds its ring — and so selection can never skip the watermark.
        match self.latch_and_advance(&ring, now) {
            Some((_selected, state)) => state,
            None => SourceState::NoSignal,
        }
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
        // Selecting the latched frame also advances the prune watermark (see
        // `latch_and_advance`), so the producer's next publish can drop consumed
        // past frames (invariant #9) without ever dropping this one. The only new
        // per-tick cost on the read path is that one relaxed-atomic `fetch_max` —
        // no lock, no allocation.
        let Some((selected, state)) = self.latch_and_advance(&ring, now) else {
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

    /// The number of frames currently retained in the media-time ring.
    ///
    /// Introspection for bounded-memory tests and soak gates (invariant #9): the
    /// ring must not accumulate frames the output clock has already advanced
    /// past. Hidden from the public docs — it exposes an internal buffer's
    /// occupancy, not a stable API contract.
    #[doc(hidden)]
    #[must_use]
    pub fn retained_frames(&self) -> usize {
        self.ring.load().len()
    }
}

#[cfg(test)]
mod cross_generation_watermark_race {
    //! Regression: a reader holding a *superseded* ring snapshot must not be able
    //! to corrupt the prune watermark of the generation that replaced it.
    //!
    //! The race (found by the ADR-T009 data-plane review): a reader
    //! ([`read_at`](TileStore::read_at) / [`state_at`](TileStore::state_at))
    //! *loads* the ring snapshot and *then* advances the prune watermark from the
    //! frame it selected. If a reconnect (a backwards re-anchor to a fresh, LOW
    //! media-time generation) lands in between, a reader still holding the OLD,
    //! HIGH media-time snapshot advances the watermark to an old high value —
    //! **after** the re-anchor. A watermark keyed on media time then lets the
    //! next forward publish on the new generation prune that generation's own
    //! frames as "older than the (stale) watermark", dropping the boundary frame
    //! the compositor is showing (invariant #2 / #9).
    //!
    //! The two barriers force exactly that interleaving deterministically —
    //! `publish(reconnect)` → stale `latch_and_advance` → `publish(forward)` —
    //! rather than relying on a flaky stress loop to hit the narrow window.
    use std::sync::{Arc, Barrier};
    use std::thread;

    use multiview_core::time::MediaTime;

    use super::{NoSignalPolicy, TileStore, TileThresholds};

    const SEC: i64 = 1_000_000_000;
    const MS: i64 = 1_000_000;

    #[test]
    fn stale_reader_cannot_prune_the_reconnected_generation() {
        // Holds frames forever, so pruning — not the failure ladder — is what
        // this probes.
        let s: Arc<TileStore<u32>> = Arc::new(TileStore::new(
            "t",
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));

        // Generation G0, stamped HIGH on the media timeline (10s, 11s).
        s.publish(1_u32, MediaTime::from_nanos(10 * SEC));
        s.publish(2_u32, MediaTime::from_nanos(11 * SEC));

        let captured = Arc::new(Barrier::new(2)); // reader has loaded the G0 snapshot
        let reconnected = Arc::new(Barrier::new(2)); // producer has re-anchored to G1

        let reader = {
            let s = Arc::clone(&s);
            let captured = Arc::clone(&captured);
            let reconnected = Arc::clone(&reconnected);
            thread::spawn(move || {
                // Load the G0 (high-time) snapshot BEFORE the reconnect — exactly
                // what a reader does at the top of `read_at`.
                let g0 = s.ring.load_full();
                captured.wait();
                // Wait until the producer has re-anchored to the new LOW
                // generation, then advance the watermark from the STALE snapshot
                // (the second half of `read_at`, now a generation behind).
                reconnected.wait();
                let _ = s.latch_and_advance(g0.as_slice(), MediaTime::from_nanos(11 * SEC));
            })
        };

        // Producer: wait for the reader to capture G0, then re-anchor to a fresh
        // LOW generation with a backwards stamp at t=0.
        captured.wait();
        s.publish(100_u32, MediaTime::from_nanos(0));
        reconnected.wait();
        // The stale cross-generation watermark advance must be visible before the
        // forward publish that would act on it (thread join = happens-before).
        reader.join().expect("reader thread panicked");

        // The next forward publish on the NEW generation. A media-time watermark
        // polluted with the stale 11s latch prunes this generation's boundary
        // frame (frame 100 @ t=0) as "older than 11s".
        s.publish(101_u32, MediaTime::from_nanos(40 * MS));

        // The reconnected generation's boundary frame MUST survive: a read at its
        // instant returns frame 100, not the later 101 (nor a slate).
        assert_eq!(
            s.read_at(MediaTime::from_nanos(0)).frame().map(|f| **f),
            Some(100),
            "a stale cross-generation watermark advance must not prune the reconnected boundary frame"
        );
        assert_eq!(
            s.read_at(MediaTime::from_nanos(40 * MS))
                .frame()
                .map(|f| **f),
            Some(101),
        );
    }
}
