//! Per-stream monotonic clamp+offset packet re-stamp (GP-6, ADR-0030 §4
//! "Re-stamp rule (#3 for the copy path)").
//!
//! A **guarded passthrough** (ADR-0030 §4) COPIES coded packets — it never
//! re-encodes — and splices a pre-baked slate in on input loss. Both the copied
//! input run and the slate run must reach the muxer with timestamps that
//! [`av_interleaved_write_frame`] will accept: DTS **strictly increasing**
//! (mp4/mov abort even on *equal* DTS; mpegts is non-strict but still rejects a
//! decrease) and `pts >= dts`. At the same time B-frame reorder must survive —
//! so the copy path must NOT use the encoder path's `out_pts = f(tick)`, which
//! re-derives PTS from the tick counter and collapses the DTS/PTS reorder gap.
//!
//! The fix is a persistent, per-stream **monotonic clamp + per-boundary offset**
//! ([`RestampAccumulator`]). Video and audio each own one (per-PID streams fail
//! independently). Within a run the raw deltas pass through untouched, so frame
//! durations and the `pts - dts` reorder gap are preserved exactly; only the
//! `offset` changes at a seam, re-anchoring the next run onto the running
//! timeline. The clamp (`last_dts + 1`) — **not** any `FFmpeg` flag — is what
//! prevents the non-monotonic-DTS abort: `avoid_negative_ts=make_zero` is only a
//! one-shot leading shift and `max_interleave_delta` is an interleave-flush
//! knob, neither guards monotonicity (see [`crate`] and `multiview-ffmpeg`'s
//! `Muxer` `AVOption` surface).
//!
//! All arithmetic is exact `i64` with saturating ops — never float fps, never a
//! lossy `as` cast — so the module compiles in the **default** (no-`ffmpeg`)
//! build and is unit/property tested there.
//!
//! [`av_interleaved_write_frame`]: https://ffmpeg.org/doxygen/trunk/group__lavf__encoding.html

/// Per-stream re-stamp state: the last emitted DTS and the current offset added
/// to raw timestamps. One instance per coded stream (video + audio each own
/// one), persisting across **both** seam boundaries (input→slate and
/// slate→input) for the lifetime of a guarded passthrough.
///
/// The accumulator is pure: [`restamp`](RestampAccumulator::restamp) maps a raw
/// `(dts, pts)` to the emitted pair and advances the state; `rebase` re-anchors
/// the offset at a seam. It performs no I/O and holds no libav state, so it lives
/// in the always-compiled default build.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestampAccumulator {
    /// The most recently emitted DTS, or `None` before the first packet.
    last_dts: Option<i64>,
    /// The value added to every raw timestamp on the current run. Changed only
    /// by [`rebase`](RestampAccumulator::rebase); steady through a run so raw
    /// deltas (durations + reorder gap) pass through untouched.
    offset: i64,
}

impl RestampAccumulator {
    /// A fresh accumulator: no emitted packet yet, zero offset (so the first run
    /// passes through unchanged until the clamp first fires).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_dts: None,
            offset: 0,
        }
    }

    /// Re-stamp one packet's raw `(dts, pts)` into the emitted `(dts, pts)`,
    /// advancing the per-stream state.
    ///
    /// Applies, in order:
    /// * `dts' = max(raw_dts + offset, last_dts + 1)` — the monotonic clamp (the
    ///   abort guard); the first packet of a stream has no `last_dts` so it is
    ///   simply `raw_dts + offset`.
    /// * `pts' = max(raw_pts + offset, dts')` — never let the muxer see
    ///   `pts < dts`, while leaving a healthy reorder gap (`raw_pts > raw_dts`)
    ///   untouched.
    /// * `last_dts = dts'`.
    ///
    /// Returns `(dts', pts')`. Arithmetic is saturating `i64`: a raw timestamp
    /// near `i64::MAX`/`MIN` saturates rather than wrapping, preserving
    /// monotonicity (a pathological raw stream is a "drop down the ladder"
    /// signal upstream, not this clamp's concern).
    // `raw_dts`/`raw_pts` (and the derived `dts`/`pts`) are the irreducible
    // domain terms of the re-stamp rule; they differ only by the canonical d/p
    // suffix, which pedantic `similar_names` flags. Renaming would diverge from
    // the ADR-0030 §4 formula and obscure the very quantities computed here.
    #[allow(clippy::similar_names)]
    #[must_use]
    pub fn restamp(&mut self, raw_dts: i64, raw_pts: i64) -> (i64, i64) {
        let shifted_dts = raw_dts.saturating_add(self.offset);
        let dts = match self.last_dts {
            Some(last) => shifted_dts.max(last.saturating_add(1)),
            None => shifted_dts,
        };
        let pts = raw_pts.saturating_add(self.offset).max(dts);
        self.last_dts = Some(dts);
        (dts, pts)
    }

    /// Re-anchor the offset at a seam boundary so the **next** emitted DTS is
    /// exactly `last_dts + 1`.
    ///
    /// `raw_dts_at_boundary` is the raw DTS of the first packet of the new run
    /// (the slate's first IDR at input→slate, or the recovery IDR at
    /// slate→input). Setting `offset = (last_dts + 1) - raw_dts_at_boundary`
    /// makes that first packet land at `last_dts + 1`, and every subsequent
    /// packet in the run inherits the same offset so its raw delta from the
    /// boundary passes through untouched (durations + reorder preserved).
    ///
    /// Before the first emitted packet there is no `last_dts`; a `rebase` then
    /// chooses the offset so the next packet lands at `0` (`last_dts` treated as
    /// `-1`), giving a deterministic zero-based start.
    pub fn rebase(&mut self, raw_dts_at_boundary: i64) {
        let target = self.last_dts.unwrap_or(-1).saturating_add(1);
        self.offset = target.saturating_sub(raw_dts_at_boundary);
    }
}
