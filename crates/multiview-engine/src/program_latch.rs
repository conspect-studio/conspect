//! The **Program latch** (`bus:program`, issue #343, `output-routing.md` §12.6):
//! a bounded ring of the most recent baked Program canvases, read by the
//! per-output clocks that run at a rate other than Program's.
//!
//! * **Publish** happens on Program's bake consumer (never on a clock thread),
//!   once per Program tick and only while a rate lane exists: one `Arc` clone of
//!   the canvas plus a copy of at most `capacity` small entries (copy-on-write,
//!   exactly like the `TileStore` media-time ring). The oldest entry is dropped
//!   when the ring is full — the ring never grows.
//! * **Latch** is the frame-sync conversion: an output tick at time `t` reads
//!   the newest entry whose Program PTS is `<= t` (nearest-not-after, whole
//!   frames, no blending). If every retained entry is after `t` (the output
//!   tick is so late that its frame was evicted) the oldest retained entry is
//!   returned and [`Latched::expired`] says so: the caller counts it, it never
//!   passes silently. An empty latch returns `None`.
//!
//! **Progress guarantee.** The ring lives in a
//! [`multiview_framestore::SwapCell`], the same cell `TileStore::read_at`
//! reads on Program's clock today. Its documented guarantee: the publisher
//! never waits on a reader; a load never blocks on a lock and never waits on a
//! paused publisher (lock-free, not wait-free), and finishes after at most
//! `k + 1` attempts where `k` is the number of publishes completing during it.
//! Here there is a single publisher (the bake consumer) publishing once per
//! Program tick, so a latch retries only when a whole publish completes
//! inside its few instructions — in practice a single attempt.
//!
//! **Capacity.** The ring must hold every frame an on-time output tick can
//! need: the output tick for time `t` runs at `t + LATCH_DELAY` (plus its wake
//! lateness), and by then Program has baked up to about `t + LATCH_DELAY`, so
//! the frame it needs is about `LATCH_DELAY × program_fps` ticks old.
//! [`ProgramLatch::capacity_for`] sizes the ring from Program's cadence as
//! `ceil(latch_delay × fps) + PROGRAM_LATCH_MARGIN` (the margin absorbs bake
//! jitter and that many Program ticks of output-clock wake lateness), clamped
//! to `[PROGRAM_LATCH_CAPACITY, MAX_PROGRAM_LATCH_CAPACITY]`. 120 fps × 100 ms
//! = 12 ticks + 16 = 28 entries; the cap (256) covers Program up to 2 400 fps
//! at the default delay (the config bounds no canvas rate, so the cap is the
//! honest limit). A tick later than the ring is an expired latch (counted).
//!
//! 30 → 25 therefore drops exactly one Program frame in six and 25 → 30 repeats
//! one in five; a Program cut lands on exactly one output frame (the first
//! output tick at or after the cut's Program PTS).

use std::sync::Arc;
use std::time::Duration;

use multiview_core::time::{MediaTime, Rational};
use multiview_framestore::SwapCell;

/// The smallest ring [`ProgramLatch::capacity_for`] returns (and the size of
/// [`ProgramLatch::new`], for tests and slow Programs).
pub const PROGRAM_LATCH_CAPACITY: usize = 8;

/// Program ticks of slack beyond `latch_delay × fps` (bake jitter + output
/// wake lateness).
pub const PROGRAM_LATCH_MARGIN: usize = 16;

/// The largest ring [`ProgramLatch::capacity_for`] returns: an entry is a
/// tick, a PTS and an `Arc` clone, so 256 is a few KiB copied per publish.
pub const MAX_PROGRAM_LATCH_CAPACITY: usize = 256;

/// One retained Program frame.
#[derive(Debug)]
struct Entry<T> {
    program_tick: u64,
    pts: MediaTime,
    frame: Arc<T>,
}

impl<T> Clone for Entry<T> {
    fn clone(&self) -> Self {
        Self {
            program_tick: self.program_tick,
            pts: self.pts,
            frame: Arc::clone(&self.frame),
        }
    }
}

/// The Program frame an output tick latched.
#[derive(Debug)]
pub struct Latched<T> {
    /// The Program tick index the frame was baked for.
    pub program_tick: u64,
    /// The Program PTS of that tick.
    pub pts: MediaTime,
    /// The baked picture (shared, never copied).
    pub frame: Arc<T>,
    /// The requested time was older than every retained entry: the returned
    /// frame is the oldest retained one and its `pts` is AFTER the request (a
    /// very late output tick, or a ring too small for Program's rate). The
    /// caller counts it; it is never silent.
    pub expired: bool,
}

impl<T> Clone for Latched<T> {
    fn clone(&self) -> Self {
        Self {
            program_tick: self.program_tick,
            pts: self.pts,
            frame: Arc::clone(&self.frame),
            expired: self.expired,
        }
    }
}

/// The bounded ring of baked Program frames (see the module docs for the
/// exact progress guarantee and the capacity derivation).
///
/// Single publisher (Program's bake consumer); any number of latching readers.
pub struct ProgramLatch<T> {
    ring: SwapCell<Arc<Vec<Entry<T>>>>,
    capacity: usize,
}

impl<T> std::fmt::Debug for ProgramLatch<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ring = self.ring.load();
        f.debug_struct("ProgramLatch")
            .field("len", &ring.len())
            .field("capacity", &self.capacity)
            .field("newest_tick", &ring.last().map(|e| e.program_tick))
            .finish_non_exhaustive()
    }
}

impl<T> Default for ProgramLatch<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ProgramLatch<T> {
    /// An empty latch of [`PROGRAM_LATCH_CAPACITY`] entries.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(PROGRAM_LATCH_CAPACITY)
    }

    /// An empty latch retaining `capacity` frames (at least 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            ring: SwapCell::new(Arc::new(Vec::with_capacity(capacity))),
            capacity,
        }
    }

    /// An empty latch sized for a Program at `program_cadence` read
    /// `latch_delay` behind ([`Self::capacity_for`]).
    #[must_use]
    pub fn for_program(program_cadence: Rational, latch_delay: Duration) -> Self {
        Self::with_capacity(Self::capacity_for(program_cadence, latch_delay))
    }

    /// The ring size for a Program at `program_cadence` read `latch_delay`
    /// behind: `ceil(latch_delay × fps) + PROGRAM_LATCH_MARGIN`, clamped to
    /// `[PROGRAM_LATCH_CAPACITY, MAX_PROGRAM_LATCH_CAPACITY]`. Integer
    /// arithmetic only (never float fps). A degenerate cadence gets the cap.
    #[must_use]
    pub fn capacity_for(program_cadence: Rational, latch_delay: Duration) -> usize {
        if program_cadence.num <= 0 || program_cadence.den <= 0 {
            return MAX_PROGRAM_LATCH_CAPACITY;
        }
        let delay_ns = i128::try_from(latch_delay.as_nanos()).unwrap_or(i128::MAX);
        let num = delay_ns.saturating_mul(i128::from(program_cadence.num));
        let den = i128::from(program_cadence.den).saturating_mul(1_000_000_000);
        let ticks = num.saturating_add(den - 1) / den;
        usize::try_from(ticks)
            .unwrap_or(usize::MAX)
            .saturating_add(PROGRAM_LATCH_MARGIN)
            .clamp(PROGRAM_LATCH_CAPACITY, MAX_PROGRAM_LATCH_CAPACITY)
    }

    /// The most frames retained.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// A shared, empty latch.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Publish the baked Program canvas for `program_tick` at `pts`.
    ///
    /// Called by the single publisher (the bake consumer). Copy-on-write: the
    /// at most `capacity` retained entries are copied (pointer
    /// clones) with the new one appended and the oldest dropped when full, then
    /// stored with one wait-free [`SwapCell::store`]. A tick older than or equal
    /// to the newest retained tick is ignored, so the ring stays strictly
    /// ascending in tick (and so in PTS).
    pub fn publish(&self, program_tick: u64, pts: MediaTime, frame: Arc<T>) {
        let current = self.ring.load();
        if current
            .last()
            .is_some_and(|newest| newest.program_tick >= program_tick || newest.pts >= pts)
        {
            return;
        }
        let keep = self.capacity.saturating_sub(1);
        let skip = current.len().saturating_sub(keep);
        let mut next = Vec::with_capacity(self.capacity);
        next.extend(current.iter().skip(skip).cloned());
        next.push(Entry {
            program_tick,
            pts,
            frame,
        });
        self.ring.store(Arc::new(next));
    }

    /// The frame an output tick at time `at` shows: the newest retained entry
    /// with `pts <= at`, else the oldest retained entry flagged
    /// [`Latched::expired`], else `None` (nothing published yet). Never blocks
    /// on the publisher (see the module docs for the exact guarantee).
    #[must_use]
    pub fn latch(&self, at: MediaTime) -> Option<Latched<T>> {
        let ring = self.ring.load();
        let first = ring.first()?;
        let idx = ring.partition_point(|e| e.pts <= at);
        let chosen = if idx == 0 {
            first
        } else {
            ring.get(idx.saturating_sub(1)).unwrap_or(first)
        };
        Some(Latched {
            program_tick: chosen.program_tick,
            pts: chosen.pts,
            frame: Arc::clone(&chosen.frame),
            expired: idx == 0,
        })
    }

    /// How many frames are retained now (at most [`Self::capacity`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.load().len()
    }

    /// Whether nothing has been published yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.load().is_empty()
    }

    /// The newest published Program tick, if any.
    #[must_use]
    pub fn newest_tick(&self) -> Option<u64> {
        self.ring.load().last().map(|e| e.program_tick)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::clock::OutputClock;
    use multiview_core::time::Rational;
    use std::collections::BTreeSet;

    fn pts(n: i64) -> MediaTime {
        MediaTime::from_nanos(n)
    }

    #[test]
    fn empty_latch_returns_none() {
        let latch: ProgramLatch<u32> = ProgramLatch::new();
        assert!(latch.is_empty());
        assert!(latch.latch(pts(0)).is_none());
        assert!(latch.latch(pts(i64::MAX)).is_none());
    }

    #[test]
    fn latch_is_nearest_not_after_else_oldest() {
        let latch = ProgramLatch::new();
        for t in 1..=4_u64 {
            latch.publish(t, pts(i64::try_from(t).unwrap() * 100), Arc::new(t));
        }
        // Exactly on an entry: that entry.
        assert_eq!(latch.latch(pts(200)).unwrap().program_tick, 2);
        // Between entries: the one not after.
        assert_eq!(latch.latch(pts(299)).unwrap().program_tick, 2);
        assert_eq!(*latch.latch(pts(301)).unwrap().frame, 3);
        // Past the newest: the newest.
        assert_eq!(latch.latch(pts(10_000)).unwrap().program_tick, 4);
        // Before the oldest: the oldest retained (never None once published).
        let early = latch.latch(pts(5)).unwrap();
        assert_eq!((early.program_tick, early.pts), (1, pts(100)));
        assert!(early.expired, "older than the ring: flagged");
        assert!(!latch.latch(pts(100)).unwrap().expired);
    }

    #[test]
    fn publishing_an_older_or_equal_tick_is_ignored() {
        let latch = ProgramLatch::new();
        latch.publish(5, pts(500), Arc::new(5_u64));
        latch.publish(5, pts(600), Arc::new(99));
        latch.publish(3, pts(300), Arc::new(98));
        assert_eq!(latch.len(), 1);
        assert_eq!(*latch.latch(pts(1_000)).unwrap().frame, 5);
        assert_eq!(latch.newest_tick(), Some(5));
    }

    #[test]
    fn the_ring_is_bounded_at_capacity_and_drops_the_oldest() {
        let latch = ProgramLatch::new();
        for t in 0..100_u64 {
            latch.publish(t, pts(i64::try_from(t).unwrap()), Arc::new(t));
            assert!(latch.len() <= PROGRAM_LATCH_CAPACITY);
        }
        assert_eq!(latch.len(), PROGRAM_LATCH_CAPACITY);
        // The oldest retained is tick 92 (100 - 8).
        assert_eq!(latch.latch(pts(0)).unwrap().program_tick, 92);
        assert_eq!(latch.newest_tick(), Some(99));
    }

    /// Drive a Program clock at `prog` and an output clock at `out` for
    /// `out_ticks` output ticks. Before each output tick latches, the publisher
    /// has baked every Program tick due up to `out_pts + lead` (the bake runs
    /// ahead of the output clock by the lane's latch delay). Returns the
    /// latched `(program_tick, program_pts, out_pts)` per output tick.
    fn convert(
        prog: Rational,
        out: Rational,
        out_ticks: u64,
        lead_ns: i64,
    ) -> Vec<(u64, i64, i64)> {
        let prog_clock = OutputClock::new(prog).unwrap();
        let out_clock = OutputClock::new(out).unwrap();
        let latch = ProgramLatch::for_program(prog, crate::output_lane::LATCH_DELAY);
        let mut next_prog = 0_u64;
        let mut seen = Vec::new();
        for k in 0..out_ticks {
            let t = out_clock.pts_at(k);
            while prog_clock.pts_at(next_prog).as_nanos() <= t.as_nanos() + lead_ns {
                latch.publish(next_prog, prog_clock.pts_at(next_prog), Arc::new(next_prog));
                next_prog += 1;
            }
            let got = latch.latch(t).unwrap();
            assert!(!got.expired, "k={k}: an on-time tick never expires");
            seen.push((got.program_tick, got.pts.as_nanos(), t.as_nanos()));
        }
        seen
    }

    /// The exact latched Program tick for output tick `k`: the largest `n`
    /// with `n / prog <= k / out`, i.e. `floor(k · out⁻¹ · prog)`, in exact
    /// integer arithmetic.
    fn expected(prog: Rational, out: Rational, k: u64) -> u64 {
        let num = i128::from(k) * i128::from(out.den) * i128::from(prog.num);
        let den = i128::from(out.num) * i128::from(prog.den);
        u64::try_from(num / den).unwrap()
    }

    #[test]
    fn thirty_to_twentyfive_drops_exactly_one_frame_in_six() {
        let (prog, out) = (Rational::FPS_30, Rational::FPS_25);
        // 10 s of output ticks; the bake runs 100 ms ahead.
        let seen = convert(prog, out, 250, 100_000_000);
        for (k, (tick, _, _)) in seen.iter().enumerate() {
            assert_eq!(*tick, expected(prog, out, u64::try_from(k).unwrap()));
        }
        // Every output second (25 ticks) shows 25 distinct Program frames of
        // the 30 baked: in each group of 6 Program frames exactly one is
        // never shown.
        let shown: BTreeSet<u64> = seen.iter().map(|s| s.0).collect();
        assert_eq!(shown.len(), 250, "no output tick repeats a Program frame");
        for group in 0..50_u64 {
            let dropped = (group * 6..group * 6 + 6)
                .filter(|n| !shown.contains(n))
                .count();
            assert_eq!(dropped, 1, "group {group}");
        }
    }

    #[test]
    fn ntsc_to_twentyfive_drops_the_exact_rational_share_and_never_shows_the_future() {
        let (prog, out) = (Rational::FPS_29_97, Rational::FPS_25);
        // 1001 output ticks span exactly 1200 Program frames (k·1200/1001).
        let seen = convert(prog, out, 1001, 100_000_000);
        for (k, (tick, prog_pts, out_pts)) in seen.iter().enumerate() {
            assert_eq!(
                *tick,
                expected(prog, out, u64::try_from(k).unwrap()),
                "k={k}"
            );
            assert!(prog_pts <= out_pts, "k={k}: latched a frame after the tick");
            // Not stale: the next Program frame is after the output tick.
            let next = OutputClock::new(prog).unwrap().pts_at(tick + 1).as_nanos();
            assert!(next > *out_pts, "k={k}: a newer frame was due");
        }
        let shown: BTreeSet<u64> = seen.iter().map(|s| s.0).collect();
        assert_eq!(shown.len(), 1001, "no repeats when Program is faster");
        // Program frames 0..1200 baked inside the window; 1001 shown, so
        // exactly 199 dropped (≈ one in six: 1200/199 = 6.03).
        let dropped = (0..1200_u64).filter(|n| !shown.contains(n)).count();
        assert_eq!(dropped, 199);
        // Steady state: consecutive drops are 5 or 6 Program frames apart.
        let drops: Vec<u64> = (0..1200_u64).filter(|n| !shown.contains(n)).collect();
        for pair in drops.windows(2) {
            let gap = pair[1] - pair[0];
            assert!((5..=7).contains(&gap), "gap {gap} between {pair:?}");
        }
    }

    #[test]
    fn twentyfive_to_thirty_repeats_exactly_one_in_five() {
        let (prog, out) = (Rational::FPS_25, Rational::FPS_30);
        let seen = convert(prog, out, 300, 100_000_000);
        for (k, (tick, prog_pts, out_pts)) in seen.iter().enumerate() {
            assert_eq!(*tick, expected(prog, out, u64::try_from(k).unwrap()));
            assert!(prog_pts <= out_pts);
        }
        // Every group of 5 Program frames is shown on 6 output ticks: exactly
        // one of the five appears twice, the others once.
        for group in 0..50_u64 {
            let counts: Vec<usize> = (group * 5..group * 5 + 5)
                .map(|n| seen.iter().filter(|s| s.0 == n).count())
                .collect();
            assert_eq!(counts.iter().sum::<usize>(), 6, "group {group}");
            assert_eq!(
                counts.iter().filter(|c| **c == 2).count(),
                1,
                "group {group}"
            );
            assert_eq!(
                counts.iter().filter(|c| **c == 1).count(),
                4,
                "group {group}"
            );
        }
    }

    #[test]
    fn a_program_cut_lands_on_exactly_one_output_frame() {
        let (prog, out) = (Rational::FPS_29_97, Rational::FPS_25);
        let prog_clock = OutputClock::new(prog).unwrap();
        let out_clock = OutputClock::new(out).unwrap();
        for cut in [37_u64, 100, 241] {
            let cut_pts = prog_clock.pts_at(cut);
            let seen = convert(prog, out, 300, 100_000_000);
            // The first output tick showing a frame at or after the cut.
            let first = seen.iter().position(|s| s.0 >= cut).unwrap();
            let first_k = u64::try_from(first).unwrap();
            assert!(out_clock.pts_at(first_k) >= cut_pts);
            assert!(first == 0 || out_clock.pts_at(first_k - 1) < cut_pts);
            assert!(seen[..first].iter().all(|s| s.0 < cut));
            assert!(seen[first..].iter().all(|s| s.0 >= cut));
        }
    }

    #[test]
    fn capacity_covers_the_latch_delay_at_high_program_rates() {
        let delay = crate::output_lane::LATCH_DELAY;
        for (fps, want) in [
            (Rational::FPS_25, 3 + PROGRAM_LATCH_MARGIN),
            (Rational::FPS_29_97, 3 + PROGRAM_LATCH_MARGIN),
            (Rational::FPS_59_94, 6 + PROGRAM_LATCH_MARGIN),
            (Rational::new(120, 1), 12 + PROGRAM_LATCH_MARGIN),
            (Rational::new(120_000, 1001), 12 + PROGRAM_LATCH_MARGIN),
        ] {
            assert_eq!(
                ProgramLatch::<u8>::capacity_for(fps, delay),
                want,
                "{fps:?}"
            );
        }
        assert_eq!(
            ProgramLatch::<u8>::capacity_for(Rational::new(1, 1), Duration::ZERO),
            PROGRAM_LATCH_MARGIN
        );
        assert_eq!(
            ProgramLatch::<u8>::capacity_for(Rational::new(1_000_000, 1), delay),
            MAX_PROGRAM_LATCH_CAPACITY
        );
        assert_eq!(
            ProgramLatch::<u8>::capacity_for(Rational::new(0, 1), delay),
            MAX_PROGRAM_LATCH_CAPACITY
        );
    }

    /// 120 fps Program latched at 25 with the real 100 ms lead: tick 0 (and
    /// every on-time tick) gets exactly floor(k · 120 / 25), never a future
    /// frame, never expired — the case a fixed 8-entry ring broke (tick 0
    /// was evicted by the time output tick 0 ran).
    #[test]
    fn a_120_fps_program_latches_the_exact_tick_with_the_real_delay() {
        let (prog, out) = (Rational::new(120, 1), Rational::FPS_25);
        let seen = convert(prog, out, 250, 100_000_000);
        for (k, (tick, prog_pts, out_pts)) in seen.iter().enumerate() {
            assert_eq!(
                *tick,
                expected(prog, out, u64::try_from(k).unwrap()),
                "k={k}"
            );
            assert!(prog_pts <= out_pts, "k={k}");
        }
        assert_eq!(seen[0].0, 0, "tick 0 still retained");
        // A cut at Program tick 241 lands on the first output tick at or
        // after it: k = ceil(241 · 25 / 120) = 51.
        let first = seen.iter().position(|s| s.0 >= 241).unwrap();
        assert_eq!(first, 51);
        assert!(seen[50].0 < 241);
    }

    /// A delayed output wake beyond the ring at 120 fps: the latch returns
    /// the oldest retained frame flagged expired; within the ring it is
    /// exact and not expired.
    #[test]
    fn a_wake_later_than_the_ring_is_an_expired_latch() {
        let prog = Rational::new(120, 1);
        let clock = OutputClock::new(prog).unwrap();
        let latch = ProgramLatch::for_program(prog, crate::output_lane::LATCH_DELAY);
        let cap = u64::try_from(latch.capacity()).unwrap();
        // Program baked through tick 60 (500 ms).
        for n in 0..=60_u64 {
            latch.publish(n, clock.pts_at(n), Arc::new(n));
        }
        let oldest = 60 + 1 - cap;
        assert!(oldest > 12);
        let got = latch.latch(clock.pts_at(oldest + 2)).unwrap();
        assert_eq!((got.program_tick, got.expired), (oldest + 2, false));
        // An output tick for t = 100 ms (Program tick 12), 400 ms late.
        let got = latch.latch(clock.pts_at(12)).unwrap();
        assert_eq!((got.program_tick, got.expired), (oldest, true));
    }

    #[test]
    fn concurrent_latches_never_block_and_always_see_an_ascending_ring() {
        let latch: Arc<ProgramLatch<u64>> = ProgramLatch::shared();
        let reader = {
            let latch = Arc::clone(&latch);
            std::thread::spawn(move || {
                let mut last = 0_u64;
                for _ in 0..200_000 {
                    if let Some(got) = latch.latch(MediaTime::from_nanos(i64::MAX)) {
                        assert!(got.program_tick >= last);
                        assert_eq!(*got.frame, got.program_tick);
                        last = got.program_tick;
                    }
                }
            })
        };
        for t in 0..50_000_u64 {
            latch.publish(
                t,
                MediaTime::from_nanos(i64::try_from(t).unwrap()),
                Arc::new(t),
            );
        }
        reader.join().unwrap();
    }
}
