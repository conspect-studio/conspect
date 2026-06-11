//! Per-layer subtitle/caption **cue re-point** — the overlay-crate core of the
//! decoupled-routing subtitle breakaway (RT-10a; ADR-0034, building on the
//! unified caption-cue store of ADR-0019).
//!
//! A subtitle *layer* on the multiview canvas does not have to take its captions
//! from the same input as the video underneath it: an operator can route
//! subtitles from a **different** source ("subtitle breakaway" — take video from
//! input A, captions from input C). The mechanic here makes that switch instant
//! and sub-frame:
//!
//! - A [`SubtitleLayer`] holds the **active cue source** behind an
//!   [`arc_swap::ArcSwap`] — an `Arc<dyn CueSource>` that exposes the same
//!   `active_at(now)` window sampling as the per-source caption cue store
//!   (ADR-0019). The compositor *samples* that source once per output tick; it
//!   never blocks on it (invariants #1/#10).
//! - [`SubtitleLayer::repoint`] atomically **replaces** the active cue source
//!   with another source's `Arc<dyn CueSource>`. Because sampling is a pure
//!   per-tick `active_at(now)` read, the re-point takes effect on the **next
//!   tick** — instant, with no reload of the overlay stack.
//! - At the seam (the first tick after a re-point) the layer performs a
//!   **CLEAR-on-switch**: it drops the cue it was latching so a stale or wide cue
//!   from the *old* source can never persist or flash over the new one. This is a
//!   **hard cut** — subtitles never cross-fade — surfaced via
//!   [`SubtitleLayer::last_was_seam`] so a renderer clears-then-draws at the seam.
//!   After the seam, the new source's `active_at(now)` cues render normally.
//!
//! ## Why a latch + an explicit clear
//!
//! `active_at(now)` is a pure point sample, so re-pointing already stops reading
//! the old source. The subtle bug this guards against is a renderer that
//! **latches** the last on-screen cue for stability (so identical cues are not
//! re-rasterized every tick): without an explicit clear, that latch would keep
//! the *old* source's last cue on screen across the switch until the new source
//! produced its first cue. [`SubtitleLayer`] models the latch and clears it at
//! the seam, so the worst case — the old source's wide cue with no new cue yet —
//! renders nothing, never the stale caption.
//!
//! ## Pure + isolated
//!
//! This module is pure Rust (no native dependency, no FFI). It depends only on
//! [`crate::subtitle::Cue`] and [`multiview_core::time::MediaTime`]. The
//! `Arc<dyn CueSource>` it samples may be backed by this crate's [`CueTrack`]
//! (sidecar SRT/VTT, which implements [`CueSource`] below) or by an
//! ingest-side caption cue store that decodes broadcast/embedded/HLS captions
//! into the same `[start, end)`-window model (ADR-0019) — anything that can
//! answer `active_at(now)` is routable here.
//!
//! [`CueTrack`]: crate::subtitle::CueTrack

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use multiview_core::time::MediaTime;

use crate::subtitle::{Cue, CueTrack};

/// A source of subtitle/caption cues sampled by output presentation time.
///
/// This is the routable unit of the subtitle breakaway: a [`SubtitleLayer`]
/// holds one of these as an `Arc<dyn CueSource>` and can be re-pointed to
/// another. The single required operation, [`CueSource::active_at`], mirrors the
/// per-source caption cue store's window sampling (ADR-0019): return the cue
/// whose `[start, end)` window contains `now` (latest-starting wins on overlap),
/// or `None` when nothing is active.
///
/// Implementors must be `Send + Sync` so a layer can be shared across the
/// command-drain thread (which re-points) and the compositor-drive thread (which
/// samples) without a lock.
pub trait CueSource: Send + Sync {
    /// The cue active at presentation time `now` (`start <= now < end`), or
    /// [`None`] if no cue covers `now`. A pure, non-blocking read.
    fn active_at(&self, now: MediaTime) -> Option<Cue>;
}

/// A [`CueTrack`] (parsed SRT/VTT sidecar, [`crate::subtitle`]) is a cue source:
/// its [`CueTrack::active_cue`] is exactly the `active_at(now)` window sample.
impl CueSource for CueTrack {
    fn active_at(&self, now: MediaTime) -> Option<Cue> {
        self.active_cue(now).cloned()
    }
}

/// A cue source that is always empty — it never has an active cue.
///
/// Used to re-point a subtitle layer onto a source that carries no captions (a
/// spare input, or "subtitles off"): the layer clears at the seam and then
/// renders nothing, rather than holding the previous source's last cue.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyCueSource;

impl CueSource for EmptyCueSource {
    fn active_at(&self, _now: MediaTime) -> Option<Cue> {
        None
    }
}

/// One subtitle/caption layer's **routable** cue binding: the active cue source
/// plus the per-tick sample latch and the seam (hard-cut) bookkeeping.
///
/// The compositor calls [`SubtitleLayer::sample`] once per output tick to get the
/// cue to render this frame. An operator re-routes the layer's captions with
/// [`SubtitleLayer::repoint`], which atomically swaps the active cue source; the
/// next [`sample`](SubtitleLayer::sample) clears the old cue and begins rendering
/// the new source's `active_at(now)` windows.
///
/// `repoint` takes `&self` (it only swaps the atomic source pointer and arms the
/// seam flag), matching the engine's command-drain at the frame boundary;
/// `sample` takes `&mut self` because it advances the per-tick display latch.
pub struct SubtitleLayer {
    /// The active cue source, atomically replaceable by [`repoint`](Self::repoint).
    ///
    /// `arc-swap` can only hold a `Sized` payload behind its `Arc`, and
    /// `dyn CueSource` is unsized, so the swappable cell stores the (Sized)
    /// `Arc<dyn CueSource>` fat pointer — a lock-free double-`Arc` that lets the
    /// command-drain replace the whole trait object atomically.
    source: ArcSwap<Arc<dyn CueSource>>,
    /// Armed by [`repoint`](Self::repoint); consumed by the next
    /// [`sample`](Self::sample) to force the CLEAR-on-switch hard cut.
    seam_armed: AtomicBool,
    /// The cue currently latched on screen — what the last
    /// [`sample`](Self::sample) returned (renderer convenience to skip
    /// re-rasterizing an unchanged cue). At a seam the next sample reads the new
    /// source, so this never carries the old source's cue forward.
    latched: Option<Cue>,
    /// Whether the most recent [`sample`](Self::sample) was the seam frame of a
    /// re-point (a hard cut). Reset to `false` on every steady (non-seam) sample.
    last_was_seam: bool,
}

impl SubtitleLayer {
    /// Create a layer initially bound to `source`.
    #[must_use]
    pub fn new(source: Arc<dyn CueSource>) -> Self {
        Self {
            source: ArcSwap::new(Arc::new(source)),
            seam_armed: AtomicBool::new(false),
            latched: None,
            last_was_seam: false,
        }
    }

    /// Re-point this layer to `source` — the subtitle breakaway switch.
    ///
    /// Atomically replaces the active cue source (replace semantics: the previous
    /// source is dropped from this layer) and arms the CLEAR-on-switch seam. The
    /// re-point itself renders nothing; the next [`sample`](Self::sample) clears
    /// the old cue and then samples `source`. Takes `&self` so it can run on the
    /// engine's command-drain at a tick boundary without `&mut` ownership.
    pub fn repoint(&self, source: Arc<dyn CueSource>) {
        self.source.store(Arc::new(source));
        self.seam_armed.store(true, Ordering::Release);
    }

    /// Sample the cue to display at output presentation time `now` (one call per
    /// output tick).
    ///
    /// Steady state: returns the active cue of the bound source
    /// (`active_at(now)`), latching it for the renderer.
    ///
    /// Seam frame (the first sample after a [`repoint`](Self::repoint)): performs
    /// the hard-cut CLEAR — the previously latched (old-source) cue is dropped, so
    /// even if the new source has no cue active at `now` the layer renders nothing
    /// rather than the stale cue. The new source's own active cue (if any) renders
    /// immediately. [`last_was_seam`](Self::last_was_seam) reports `true` for this
    /// frame so a renderer clears-then-draws instead of cross-fading.
    pub fn sample(&mut self, now: MediaTime) -> Option<Cue> {
        // Consume the seam flag: a re-point since the last sample makes this a
        // hard cut. `swap` clears it so the seam fires exactly once.
        let is_seam = self.seam_armed.swap(false, Ordering::AcqRel);
        self.last_was_seam = is_seam;
        // CLEAR-on-switch: re-point swapped the source, so this samples the NEW
        // source via its `active_at(now)` window — the old source is never read
        // again, and the latch below is overwritten with the new result. So even
        // if the old source's wide cue would still be "active", it cannot persist
        // or flash past the re-point: the new source decides what shows (possibly
        // nothing). `is_seam` only marks this frame as a hard cut for the renderer.
        let active = self.source.load().active_at(now);
        self.latched.clone_from(&active);
        active
    }

    /// Whether the most recent [`sample`](Self::sample) was a re-point seam frame
    /// (a hard cut). `false` for steady ticks and before the first sample.
    #[must_use]
    pub const fn last_was_seam(&self) -> bool {
        self.last_was_seam
    }

    /// The cue currently latched on screen — what the last [`sample`](Self::sample)
    /// returned, or [`None`] before the first sample / after a seam clear with no
    /// new cue. The renderer reuses this to avoid re-rasterizing an unchanged cue.
    #[must_use]
    pub const fn latched(&self) -> Option<&Cue> {
        self.latched.as_ref()
    }
}

impl fmt::Debug for SubtitleLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn CueSource` is not `Debug`; surface the routable bookkeeping only.
        f.debug_struct("SubtitleLayer")
            .field("seam_armed", &self.seam_armed.load(Ordering::Relaxed))
            .field("last_was_seam", &self.last_was_seam)
            .field("latched", &self.latched)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mt(secs: i64) -> MediaTime {
        MediaTime::from_nanos(secs.saturating_mul(1_000_000_000))
    }

    fn track(start_s: i64, end_s: i64, text: &str) -> Arc<dyn CueSource> {
        let doc = format!("1\n00:00:{start_s:02},000 --> 00:00:{end_s:02},000\n{text}\n");
        let parsed = CueTrack::parse_srt(&doc).unwrap_or_else(|_| {
            // A test fixture should always parse; surface a clear failure if not.
            panic!("test srt fixture failed to parse: {doc:?}")
        });
        Arc::new(parsed)
    }

    #[test]
    fn empty_source_never_has_an_active_cue() {
        assert!(EmptyCueSource.active_at(mt(0)).is_none());
        assert!(EmptyCueSource.active_at(mt(1_000)).is_none());
    }

    #[test]
    fn steady_layer_samples_the_bound_source_window() {
        let mut layer = SubtitleLayer::new(track(2, 4, "x"));
        assert!(layer.sample(mt(1)).is_none());
        assert_eq!(layer.sample(mt(3)).map(|c| c.text()), Some("x".to_owned()));
        assert!(!layer.last_was_seam());
        assert!(layer.sample(mt(5)).is_none());
    }

    #[test]
    fn repoint_clears_the_old_latched_cue_at_the_seam() {
        let mut layer = SubtitleLayer::new(track(0, 100, "old"));
        assert_eq!(
            layer.sample(mt(1)).map(|c| c.text()),
            Some("old".to_owned())
        );
        assert!(layer.latched().is_some());

        layer.repoint(track(50, 60, "new"));
        // Seam at t=2: old wide cue would still be "active" on the old source, but
        // we read the new source (no cue yet) and clear → nothing on screen.
        assert!(layer.sample(mt(2)).is_none());
        assert!(layer.last_was_seam());
        assert!(layer.latched().is_none());
    }

    #[test]
    fn repoint_shows_new_source_cue_immediately_when_active_at_the_seam() {
        let mut layer = SubtitleLayer::new(track(1, 9, "a"));
        let _ = layer.sample(mt(2));
        layer.repoint(track(1, 9, "b"));
        assert_eq!(layer.sample(mt(3)).map(|c| c.text()), Some("b".to_owned()));
        assert!(layer.last_was_seam());
    }

    #[test]
    fn seam_fires_exactly_once_then_steady() {
        let mut layer = SubtitleLayer::new(track(0, 100, "a"));
        layer.repoint(track(0, 100, "b"));
        let _ = layer.sample(mt(1));
        assert!(layer.last_was_seam());
        let _ = layer.sample(mt(2));
        assert!(!layer.last_was_seam());
    }
}
