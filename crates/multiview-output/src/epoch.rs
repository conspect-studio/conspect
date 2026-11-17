//! The shared **outbound presentation epoch** cell (ADR-M010, DEV-C1).
//!
//! One [`WallClockRef`] per program maps output-PTS nanoseconds to disciplined
//! wall-clock nanoseconds. The engine-side sampler derives and re-anchors it
//! (off the hot path, ~1 Hz) and **writes** it here; the egress consumers in
//! this crate **read** it:
//!
//! * the HLS [`LivePlaylist`](crate::hls::LivePlaylist) stamps
//!   `EXT-X-PROGRAM-DATE-TIME` from `epoch.wall_at(segment first PTS)`;
//! * the RTCP [`SrStamper`](crate::rtcp::SrStamper) stamps Sender-Report
//!   NTP↔RTP pairs from the same map.
//!
//! One anchor, every surface agrees — the load-bearing property of ADR-M010.
//!
//! ## The step seam (generation counter)
//!
//! Hold/slew re-anchors keep one continuous map; a **step** (the sampler's
//! gross-discontinuity re-anchor, `EpochUpdate::Stepped`) is the
//! Class-2-like case wall-clock-sync §3 documents: downstream timelines built
//! on the old map are discontinuous with the new one, so HLS must mark the
//! seam with `EXT-X-DISCONTINUITY`. The cell therefore carries a
//! **generation** counter: [`SharedEpoch::set`] publishes within the current
//! generation (hold/slew), [`SharedEpoch::set_stepped`] bumps it, and the
//! [`LivePlaylist`](crate::hls::LivePlaylist) compares generations at each
//! segment close to mark exactly the first segment closed under a stepped
//! map.
//!
//! ## Isolation (invariants #1/#10)
//!
//! The cell is read on egress threads only (the segmenter at segment-close
//! cadence, the RTSP seam when a report is built) — never on the output-clock
//! loop — and written at ~1 Hz by the sampler task. The interior `RwLock`
//! guards a tiny `Copy` value, so both sides hold it for nanoseconds; a
//! poisoned lock (a panicked writer elsewhere) degrades to the last stored
//! value rather than propagating the panic.
//!
//! ## Control-plane readers (ADR-0114 slice 2)
//!
//! A reader driven by a client (the cast ladder route's clock pair) must not
//! share the `RwLock` with the sampler and the sink threads: a reader that is
//! descheduled while holding it would delay the writer, and the sinks queued
//! behind the writer. Every publish therefore also stores the same value in a
//! lock-free [`SwapCell`] mirror, and [`SharedEpoch::snapshot`] reads only
//! that: it never takes the lock and never waits on the writer.

use std::sync::{Arc, RwLock};

use multiview_core::wallclock::WallClockRef;
use multiview_framestore::SwapCell;

/// The guarded cell contents: the latest epoch plus its generation (bumped
/// only by a stepped re-anchor — see the module docs).
#[derive(Debug, Clone, Copy, Default)]
struct EpochCell {
    epoch: Option<WallClockRef>,
    generation: u64,
}

/// A shared, latest-wins cell carrying the program's outbound presentation
/// epoch (`None` until the sampler first anchors it — consumers emit no
/// fabricated wall mapping before then), plus the step-seam generation
/// counter the HLS discontinuity marking rides on.
#[derive(Debug, Clone, Default)]
pub struct SharedEpoch {
    cell: Arc<RwLock<EpochCell>>,
    /// The same value, lock-free, for client-driven readers (see the module
    /// docs). Written inside the write lock, so it never runs behind it.
    mirror: Arc<Mirror>,
}

/// The lock-free mirror of the cell.
struct Mirror(SwapCell<Option<Arc<EpochCell>>>);

impl Default for Mirror {
    fn default() -> Self {
        Self(SwapCell::empty())
    }
}

impl std::fmt::Debug for Mirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mirror")
    }
}

impl SharedEpoch {
    /// An empty cell (no epoch published yet, generation 0).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the latest epoch **within the current generation** (latest-wins;
    /// overwrites any previous map). Use for anchored/held/slewed updates —
    /// one continuous map, no downstream discontinuity.
    pub fn set(&self, epoch: WallClockRef) {
        let mut guard = match self.cell.write() {
            Ok(guard) => guard,
            // A poisoned lock means a panic elsewhere while writing; the data
            // is a Copy value and always coherent — keep serving it.
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.epoch = Some(epoch);
        self.mirror.0.store(Some(Arc::new(*guard)));
    }

    /// Publish a **stepped** epoch: the sampler re-anchored across a gross
    /// discontinuity (`EpochUpdate::Stepped`), so the new map is discontinuous
    /// with the old one. Bumps the generation so segment-close consumers mark
    /// the seam (`EXT-X-DISCONTINUITY` — wall-clock-sync §3).
    pub fn set_stepped(&self, epoch: WallClockRef) {
        let mut guard = match self.cell.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.epoch = Some(epoch);
        // Wrapping is fine: consumers compare generations for INequality only,
        // and 2^64 steps cannot occur in a run's lifetime anyway.
        guard.generation = guard.generation.wrapping_add(1);
        self.mirror.0.store(Some(Arc::new(*guard)));
    }

    /// The latest epoch and its generation from the lock-free mirror (see
    /// the module docs): never takes the lock, never waits on a writer. For
    /// readers a client can drive; `None` before the first publish.
    #[must_use]
    pub fn snapshot(&self) -> Option<(WallClockRef, u64)> {
        let cell = self.mirror.0.load()?;
        cell.epoch.map(|epoch| (epoch, cell.generation))
    }

    /// Read the latest epoch, or `None` when nothing was published yet.
    #[must_use]
    pub fn get(&self) -> Option<WallClockRef> {
        self.read_cell().epoch
    }

    /// Read the latest epoch together with its generation, or `None` when
    /// nothing was published yet. Consumers that must mark the step seam
    /// (the HLS playlist) remember the generation of the last update they
    /// acted on and treat a changed generation as a discontinuity.
    #[must_use]
    pub fn get_with_generation(&self) -> Option<(WallClockRef, u64)> {
        let cell = self.read_cell();
        cell.epoch.map(|epoch| (epoch, cell.generation))
    }

    /// Read the whole guarded cell (poison-tolerant — see the module docs).
    fn read_cell(&self) -> EpochCell {
        match self.cell.read() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use multiview_core::time::Rational;

    /// The lock-free mirror follows every publish, generation included, and
    /// answers while a writer holds the lock (it never takes it).
    #[test]
    fn the_snapshot_mirrors_every_publish_without_the_lock() {
        let epoch = SharedEpoch::new();
        assert!(epoch.snapshot().is_none());
        let a = WallClockRef::new(10, 0, Rational::new(1_000_000_000, 1));
        epoch.set(a);
        assert_eq!(
            epoch.snapshot().map(|(e, g)| (e.wall_at_anchor_ns, g)),
            Some((10, 0))
        );
        let b = WallClockRef::new(20, 0, Rational::new(1_000_000_000, 1));
        epoch.set_stepped(b);
        assert_eq!(
            epoch.snapshot().map(|(e, g)| (e.wall_at_anchor_ns, g)),
            Some((20, 1))
        );
        assert_eq!(
            epoch.snapshot().map(|(e, g)| (e.wall_at_anchor_ns, g)),
            epoch
                .get_with_generation()
                .map(|(e, g)| (e.wall_at_anchor_ns, g))
        );
        // Hold the write lock: the snapshot still answers at once.
        let held = epoch.cell.write().unwrap();
        assert_eq!(epoch.snapshot().map(|(_, g)| g), Some(1));
        drop(held);
    }
}
