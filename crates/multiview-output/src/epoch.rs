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

use std::sync::{Arc, RwLock};

use multiview_core::wallclock::WallClockRef;

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
