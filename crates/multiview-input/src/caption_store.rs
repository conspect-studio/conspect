//! Per-source caption cue store: the lock-free hand-off between a caption
//! decoder (writer) and the off-hot-path overlay baker (reader).
//!
//! Captions are *sampled* at the output tick exactly like frames: the baker asks
//! for the cue active at the output presentation time. Because a caption reader
//! (e.g. a separate `WebVTT` rendition demux) runs AHEAD of the output, a single
//! "latest" slot is wrong — a just-decoded *future* cue would hide the one that
//! should currently be on screen. So the store keeps a small, bounded,
//! media-time-ordered window of cues and [`CaptionCueStore::active_at`] returns
//! the cue whose `[start, end)` window contains `now` (latest-starting wins on
//! overlap), mirroring the frame store's latch-on-tick sampling.
//!
//! The store is generic over an opaque payload `C`; the caller supplies each
//! cue's `[start, end)` window explicitly, so this module needs no caption type
//! and stays pure + testable without the `ffmpeg` feature. It is lock-free
//! (`ArcSwap`, read-copy-update): the writer never blocks the reader and the
//! reader never blocks the writer — nothing here can pace or back-pressure the
//! engine, which never touches this store (invariants #1/#10).

use std::sync::Arc;

use arc_swap::ArcSwap;
use multiview_core::time::MediaTime;

/// Default number of cues retained. Captions are sparse (seconds apart), so a
/// small window comfortably covers the read-ahead between decode and display.
const DEFAULT_CAPACITY: usize = 16;

/// One stored cue: its active window plus the opaque payload to display.
#[derive(Debug, Clone)]
struct Entry<C> {
    start: MediaTime,
    end: MediaTime,
    cue: C,
}

/// A lock-free, bounded, media-time-ordered store of caption cues for one
/// source. Cheap to clone-on-write because cues are sparse.
#[derive(Debug)]
pub struct CaptionCueStore<C> {
    entries: ArcSwap<Vec<Entry<C>>>,
    capacity: usize,
}

impl<C: Clone> CaptionCueStore<C> {
    /// Create an empty store with the default retention window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create an empty store retaining at most `capacity` cues (clamped to ≥ 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: ArcSwap::from_pointee(Vec::new()),
            capacity: capacity.max(1),
        }
    }

    /// Publish a decoded cue active over `[start, end)`.
    ///
    /// An empty or inverted window (`end <= start`) is ignored. Insertion keeps
    /// the buffer sorted by `start`; once `capacity` is exceeded the oldest cue
    /// is dropped (drop-oldest, never grow — bounded memory).
    pub fn publish(&self, start: MediaTime, end: MediaTime, cue: C) {
        if end <= start {
            return;
        }
        let current = self.entries.load();
        let mut next: Vec<Entry<C>> = current.as_ref().clone();
        let pos = next
            .iter()
            .position(|e| e.start > start)
            .unwrap_or(next.len());
        next.insert(pos, Entry { start, end, cue });
        while next.len() > self.capacity {
            next.remove(0);
        }
        self.entries.store(Arc::new(next));
    }

    /// The cue active at `now` (`start <= now < end`), latest-starting on
    /// overlap; `None` if no cue covers `now`. A pure read that never blocks the
    /// writer.
    #[must_use]
    pub fn active_at(&self, now: MediaTime) -> Option<C> {
        let entries = self.entries.load();
        entries
            .iter()
            .rev() // ascending by start → reverse yields latest-starting first
            .find(|e| e.start <= now && now < e.end)
            .map(|e| e.cue.clone())
    }

    /// Drop cues that ended at or before `watermark`, so a long-running source's
    /// bounded buffer holds live cues rather than history. A no-op (no
    /// allocation) when nothing has expired.
    pub fn prune_before(&self, watermark: MediaTime) {
        let current = self.entries.load();
        if current.iter().all(|e| e.end > watermark) {
            return;
        }
        let next: Vec<Entry<C>> = current
            .iter()
            .filter(|e| e.end > watermark)
            .cloned()
            .collect();
        self.entries.store(Arc::new(next));
    }
}

impl<C: Clone> Default for CaptionCueStore<C> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Build a `MediaTime` from a nanosecond count (test ergonomics).
    fn mt(ns: i64) -> MediaTime {
        MediaTime::from_nanos(ns)
    }

    #[test]
    fn active_at_returns_the_cue_covering_now_with_a_half_open_window() {
        let store = CaptionCueStore::new();
        store.publish(mt(1_000), mt(3_000), "hello");
        assert_eq!(store.active_at(mt(500)), None); // before start
        assert_eq!(store.active_at(mt(1_000)), Some("hello")); // start inclusive
        assert_eq!(store.active_at(mt(2_999)), Some("hello"));
        assert_eq!(store.active_at(mt(3_000)), None); // end exclusive
    }

    #[test]
    fn read_ahead_future_cue_does_not_hide_the_currently_active_one() {
        // The reader publishes a future cue ahead of display; a single "latest"
        // slot would wrongly show it. The store must still return the present cue
        // at `now` and the future cue only once `now` reaches its window.
        let store = CaptionCueStore::new();
        store.publish(mt(1_000), mt(2_000), "now");
        store.publish(mt(5_000), mt(6_000), "future");
        assert_eq!(store.active_at(mt(1_500)), Some("now"));
        assert_eq!(store.active_at(mt(3_000)), None); // gap between cues
        assert_eq!(store.active_at(mt(5_500)), Some("future"));
    }

    #[test]
    fn overlapping_cues_prefer_the_latest_starting() {
        let store = CaptionCueStore::new();
        store.publish(mt(0), mt(10_000), "wide");
        store.publish(mt(2_000), mt(4_000), "narrow");
        assert_eq!(store.active_at(mt(3_000)), Some("narrow")); // latest start wins
        assert_eq!(store.active_at(mt(5_000)), Some("wide")); // narrow has ended
    }

    #[test]
    fn empty_or_inverted_windows_are_ignored() {
        let store = CaptionCueStore::new();
        store.publish(mt(1_000), mt(1_000), "empty");
        store.publish(mt(3_000), mt(2_000), "inverted");
        assert_eq!(store.active_at(mt(1_000)), None);
        assert_eq!(store.active_at(mt(2_500)), None);
    }

    #[test]
    fn capacity_drops_the_oldest_and_never_grows() {
        let store = CaptionCueStore::with_capacity(2);
        store.publish(mt(0), mt(1_000), "a");
        store.publish(mt(2_000), mt(3_000), "b");
        store.publish(mt(4_000), mt(5_000), "c"); // evicts the oldest ("a")
        assert_eq!(store.active_at(mt(500)), None); // "a" dropped
        assert_eq!(store.active_at(mt(2_500)), Some("b"));
        assert_eq!(store.active_at(mt(4_500)), Some("c"));
    }

    #[test]
    fn prune_before_drops_only_ended_cues() {
        let store = CaptionCueStore::new();
        store.publish(mt(0), mt(1_000), "old");
        store.publish(mt(2_000), mt(3_000), "keep");
        store.prune_before(mt(1_500));
        assert_eq!(store.active_at(mt(500)), None);
        assert_eq!(store.active_at(mt(2_500)), Some("keep"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// A single cue is returned exactly within its half-open window and never
        /// outside it, for any probe time.
        #[test]
        fn a_cue_is_never_returned_outside_its_window(
            start in 0i64..1_000_000,
            len in 1i64..100_000,
            probe in -10_000i64..2_000_000,
        ) {
            let store = CaptionCueStore::new();
            let end = start.saturating_add(len);
            store.publish(mt(start), mt(end), 1u32);
            let got = store.active_at(mt(probe));
            if probe >= start && probe < end {
                prop_assert_eq!(got, Some(1u32));
            } else {
                prop_assert_eq!(got, None);
            }
        }
    }
}
