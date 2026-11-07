//! A lock-free, fixed-size, single-writer log of `(a, b, c)` triples used to
//! measure on-air A/V offset: the video ingest logs `(output_ns, raw_ns, 0)` for
//! every published frame, the audio placer logs `(at_frame, frames, raw_ns)` for
//! every placed block. A reader (the bake consumer) looks up what is on air at
//! an output instant. Writes never wait for a reader and readers never block
//! (invariants #1/#10); a slot being rewritten during a read is skipped
//! (per-slot sequence counter), never read torn.

use std::sync::atomic::{fence, AtomicI64, AtomicU64, Ordering};

/// Entries retained (about 2 minutes of 30 fps video, or several seconds of
/// heavily re-placed audio).
pub const SYNC_LOG_LEN: usize = 4096;

#[derive(Debug, Default)]
struct Slot {
    seq: AtomicU64,
    a: AtomicI64,
    b: AtomicI64,
    c: AtomicI64,
}

/// The log. One writer; any number of readers.
#[derive(Debug)]
pub struct SyncLog {
    slots: Vec<Slot>,
    written: AtomicU64,
}

impl Default for SyncLog {
    fn default() -> Self {
        Self {
            slots: (0..SYNC_LOG_LEN).map(|_| Slot::default()).collect(),
            written: AtomicU64::new(0),
        }
    }
}

impl SyncLog {
    /// Append one triple (single writer only).
    pub fn push(&self, first: i64, second: i64, third: i64) {
        let count = self.written.load(Ordering::Relaxed);
        let len = u64::try_from(self.slots.len()).unwrap_or(1).max(1);
        let Some(slot) = usize::try_from(count % len)
            .ok()
            .and_then(|i| self.slots.get(i))
        else {
            return;
        };
        let seq = slot.seq.load(Ordering::Relaxed);
        slot.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        slot.a.store(first, Ordering::Relaxed);
        slot.b.store(second, Ordering::Relaxed);
        slot.c.store(third, Ordering::Relaxed);
        slot.seq.store(seq.wrapping_add(2), Ordering::Release);
        self.written.store(count.wrapping_add(1), Ordering::Release);
    }

    /// Visit retained triples newest-first until `f` returns `Some`.
    pub fn find_newest<R>(&self, mut visit: impl FnMut(i64, i64, i64) -> Option<R>) -> Option<R> {
        let count = self.written.load(Ordering::Acquire);
        let len = u64::try_from(self.slots.len()).unwrap_or(1).max(1);
        for k in (count.saturating_sub(len)..count).rev() {
            let Some(slot) = usize::try_from(k % len)
                .ok()
                .and_then(|i| self.slots.get(i))
            else {
                continue;
            };
            let s1 = slot.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                continue;
            }
            let (first, second, third) = (
                slot.a.load(Ordering::Relaxed),
                slot.b.load(Ordering::Relaxed),
                slot.c.load(Ordering::Relaxed),
            );
            fence(Ordering::Acquire);
            if slot.seq.load(Ordering::Relaxed) != s1 {
                continue;
            }
            if let Some(found) = visit(first, second, third) {
                return Some(found);
            }
        }
        None
    }

    /// Video: the raw PTS (ns) of the newest frame stamped at or before output
    /// time `now_ns` (the tile's latch rule: nearest-but-not-after).
    #[must_use]
    pub fn video_raw_at(&self, now_ns: i64) -> Option<i64> {
        self.find_newest(|at, raw, _| (at <= now_ns).then_some(raw))
    }

    /// Audio: the raw PTS (ns) of the sample at output frame `frame`, from the
    /// newest placement covering it; `None` if the newest relevant record is a
    /// clear (the frame reads silence).
    #[must_use]
    pub fn audio_raw_at(&self, frame: i64, rate: u32) -> Option<i64> {
        self.find_newest(|at, frames, raw| {
            if frames == 0 {
                // A clear: everything from `at` on was dropped at that moment.
                return (frame >= at).then_some(None);
            }
            (at <= frame && frame < at.saturating_add(frames)).then(|| {
                let off = i128::from(frame - at) * 1_000_000_000 / i128::from(rate.max(1));
                Some(raw.saturating_add(i64::try_from(off).unwrap_or(0)))
            })
        })
        .flatten()
    }

    /// Audio: record a block placed at `at` (frames) for `frames` frames whose
    /// first sample is raw `raw_ns`.
    pub fn placed(&self, at: i64, frames: i64, raw_ns: i64) {
        if frames > 0 {
            self.push(at, frames, raw_ns);
        }
    }

    /// Audio: record that everything at frames `>= at` was cleared.
    pub fn cleared(&self, at: i64) {
        self.push(at, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn video_lookup_follows_the_latch_rule() {
        let log = SyncLog::default();
        for i in 0..10 {
            log.push(i * 100, 5_000 + i, 0);
        }
        assert_eq!(log.video_raw_at(450), Some(5_004));
        assert_eq!(log.video_raw_at(-1), None);
    }

    #[test]
    fn audio_lookup_uses_the_newest_placement_and_honours_clears() {
        let log = SyncLog::default();
        log.placed(0, 48_000, 1_000_000_000);
        assert_eq!(log.audio_raw_at(24_000, 48_000), Some(1_500_000_000));
        log.cleared(12_000);
        assert_eq!(log.audio_raw_at(24_000, 48_000), None);
        assert_eq!(log.audio_raw_at(6_000, 48_000), Some(1_125_000_000));
        log.placed(12_000, 48_000, 9_000_000_000);
        assert_eq!(log.audio_raw_at(24_000, 48_000), Some(9_250_000_000));
    }

    #[test]
    fn old_entries_are_overwritten_not_grown() {
        let log = SyncLog::default();
        let len = i64::try_from(SYNC_LOG_LEN).unwrap();
        for i in 0..(len * 2) {
            log.push(i, i, 0);
        }
        assert_eq!(log.video_raw_at(10), None, "evicted");
        assert_eq!(log.video_raw_at(i64::MAX), Some(len * 2 - 1));
    }
}
