//! Per-input PTS normalization (invariant #3, the unified timing model).
//!
//! Every input is sampled, never pacing. Before its frames reach the frame
//! store, each input's raw presentation timestamps pass through this
//! normalizer, which:
//!
//! 1. **unwraps** wrapped timestamps (33-bit MPEG-TS, 32-bit RTP) with a
//!    *delta-based* algorithm into a 64-bit counter — never a value compare,
//!    and never libavformat's `pts_wrap_reference` heuristic (which has misfired
//!    in production; see [ADR-T003]);
//! 2. provides a **genpts-equivalent fallback** when the raw PTS is missing
//!    (`AV_NOPTS_VALUE`), synthesizing it from the declared cadence;
//! 3. **rebases** the unwrapped timestamp onto Multiview's internal monotonic
//!    nanosecond timeline via [`multiview_core::time::rescale`], anchored so the
//!    first frame lands at the master clock's "now";
//! 4. **re-anchors** smoothly on a discontinuity (`EXT-X-DISCONTINUITY`, a TS
//!    discontinuity indicator, or a raw-PTS jump beyond a threshold) so the
//!    output continues forward instead of skipping hours or stalling;
//! 5. enforces a **monotonic guard**: the emitted nanosecond timestamp is always
//!    strictly greater than the previous one, so a downstream muxer never aborts
//!    on a non-monotonic timestamp.
//!
//! All arithmetic is exact integer math (i64 ns / i128 intermediates / exact
//! rationals) — **never** float fps, which drifts ~3.6 s/hour for the NTSC
//! `1001` family.
//!
//! [ADR-T003]: per-input timestamp normalization.
use crate::error::{Error, Result};
use multiview_core::time::{rescale, MediaTime, Rational};

/// The timestamp wrap width of an input stream.
///
/// MPEG-TS PTS/DTS are 33-bit (wrap ~26.5 h at 90 kHz); RTP timestamps are
/// 32-bit (wrap ~13.25 h at 90 kHz). The width selects the modulus used by the
/// delta-based unwrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WrapBits {
    /// 33-bit MPEG-TS PTS/DTS timestamps.
    Mpeg33,
    /// 32-bit RTP timestamps.
    Rtp32,
    /// No wrapping (e.g. a container that already exposes a 64-bit monotonic
    /// timestamp).
    None,
}

impl WrapBits {
    /// The modulus `2^bits` for this wrap width, or `None` for [`WrapBits::None`].
    #[must_use]
    const fn modulus(self) -> Option<i64> {
        match self {
            Self::Mpeg33 => Some(1_i64 << 33),
            Self::Rtp32 => Some(1_i64 << 32),
            Self::None => None,
        }
    }

    /// Half the modulus, the threshold below which a negative delta is treated
    /// as a forward wrap.
    #[must_use]
    const fn half_modulus(self) -> Option<i64> {
        match self {
            Self::Mpeg33 => Some(1_i64 << 32),
            Self::Rtp32 => Some(1_i64 << 31),
            Self::None => None,
        }
    }
}

/// Default discontinuity threshold: a raw-PTS jump larger than this many
/// nanoseconds (in either direction) is treated as a timeline break and triggers
/// a smooth re-anchor rather than being propagated. ~10 s, per ADR-T003.
const DEFAULT_DISCONTINUITY_NS: i64 = 10_000_000_000;

/// The nanosecond timebase (`1 / 1_000_000_000` s).
const fn ns_timebase() -> Rational {
    Rational::new(1, 1_000_000_000)
}

/// Immutable mapping from a real input PTS to the internal media timeline.
///
/// This value is independent of the normalizer: projecting announcements never
/// advances video state. It carries no source identity or epoch. Callers must
/// associate it with their source/epoch and discard saved copies on discontinuity,
/// reconnect or source replacement; an existing copy cannot be revoked.
#[derive(Debug, Clone, Copy)]
pub struct PtsMappingSnapshot {
    raw: i64,
    timebase: Rational,
    media_ns: i64,
    generation: u64,
}

impl PtsMappingSnapshot {
    /// Mapping generation within this normalizer's lifetime, not a source epoch.
    /// Compare with [`PtsNormalizer::mapping_generation`] before reusing a copy.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Project an event using its own timebase and wrap width (for SCTE-35,
    /// `1/90000` and [`WrapBits::Mpeg33`]), without touching video state.
    ///
    /// Wrapped values must be in `[0, modulus)`. The nearest wrap occurrence
    /// relative to the anchor is selected; exactly half a modulus is ambiguous
    /// and rejected. The caller must guarantee the event is less than half a
    /// wrap period from this anchor: more distant occurrences are unknowable.
    /// Nanoseconds round to nearest, ties away from zero, using checked integer
    /// arithmetic. Returns `None` for invalid/non-positive timebases, ambiguous
    /// wraps, or arithmetic/result overflow; never saturates or fabricates time.
    #[must_use]
    pub fn project(&self, raw: i64, timebase: Rational, wrap: WrapBits) -> Option<MediaTime> {
        if timebase.num <= 0 || timebase.den <= 0 {
            return None;
        }
        // Exact delta in event ticks; do not quantize the anchor to event ticks.
        let denominator = i128::from(self.timebase.den).checked_mul(i128::from(timebase.num))?;
        let anchor = i128::from(self.raw)
            .checked_mul(i128::from(self.timebase.num))?
            .checked_mul(i128::from(timebase.den))?;
        let mut delta = i128::from(raw)
            .checked_mul(denominator)?
            .checked_sub(anchor)?;
        if let Some(modulus) = wrap.modulus() {
            if !(0..modulus).contains(&raw) {
                return None;
            }
            let period = i128::from(modulus).checked_mul(denominator)?;
            delta = delta.rem_euclid(period);
            let half = period / 2;
            if delta == half {
                return None;
            }
            if delta > half {
                delta = delta.checked_sub(period)?;
            }
        }
        // Cancel the event numerator algebraically before conversion to ns.
        let numerator = delta.checked_mul(1_000_000_000)?;
        let divisor = i128::from(self.timebase.den).checked_mul(i128::from(timebase.den))?;
        let ns = checked_round(numerator, divisor)?;
        let media_ns = i128::from(self.media_ns).checked_add(ns)?;
        Some(MediaTime::from_nanos(i64::try_from(media_ns).ok()?))
    }
}

fn checked_round(numerator: i128, denominator: i128) -> Option<i128> {
    let half = denominator / 2;
    let rounded = if numerator < 0 {
        numerator.checked_sub(half)?
    } else {
        numerator.checked_add(half)?
    };
    rounded.checked_div(denominator)
}

fn checked_raw_ns(raw: i64, timebase: Rational) -> Option<i64> {
    if timebase.num <= 0 || timebase.den <= 0 {
        return None;
    }
    let numerator = i128::from(raw)
        .checked_mul(i128::from(timebase.num))?
        .checked_mul(1_000_000_000)?;
    i64::try_from(checked_round(numerator, i128::from(timebase.den))?).ok()
}

/// Per-input timestamp normalizer.
///
/// Construct one per input with [`PtsNormalizer::new`], then feed every decoded
/// frame's raw timestamp (in the input's timebase) to [`PtsNormalizer::normalize`].
/// Signal a known discontinuity with [`PtsNormalizer::mark_discontinuity`].
///
/// The normalizer carries no float state; its emitted [`MediaTime`] values are
/// strictly increasing nanosecond instants on the internal timeline.
#[derive(Debug)]
pub struct PtsNormalizer {
    wrap: WrapBits,
    /// Input timebase (seconds per raw tick), e.g. `1/90000` for MPEG-TS.
    timebase: Rational,
    /// Declared cadence (fps) used for the genpts fallback frame period.
    cadence: Rational,
    /// One frame period in nanoseconds, derived from `cadence` (exact rational).
    frame_period_ns: i64,
    /// Discontinuity threshold in nanoseconds.
    discontinuity_ns: i64,
    /// Accumulated wrap offset (in raw ticks) added to every raw value.
    accumulated_wrap: i64,
    /// The previous raw (masked) value seen, for delta-based wrap detection.
    last_raw: Option<i64>,
    /// The previous unwrapped-and-rebased nanosecond value *before* rebasing
    /// (i.e. raw timeline in ns), for discontinuity detection.
    last_raw_ns: Option<i64>,
    /// The rebase offset: `media_time = raw_ns + offset`.
    offset: i64,
    /// The previous emitted media time in ns, for the monotonic guard.
    last_media_ns: Option<i64>,
    /// Whether a discontinuity has been explicitly flagged for the next frame.
    pending_discontinuity: bool,
    /// Latest usable real-PTS anchor; genpts never creates a mapping.
    mapping: Option<PtsMappingSnapshot>,
    /// None permanently disables mapping after generation exhaustion.
    mapping_generation: Option<u64>,
}

impl PtsNormalizer {
    /// Construct a normalizer for an input with the given wrap width, input
    /// `timebase` (seconds per raw tick), and declared `cadence` (fps) used for
    /// the genpts fallback.
    ///
    /// The discontinuity threshold defaults to ~10 s; override it with
    /// [`PtsNormalizer::with_discontinuity_ns`].
    #[must_use]
    pub fn new(wrap: WrapBits, timebase: Rational, cadence: Rational) -> Self {
        // Frame period (ns) = 1 / cadence seconds, expressed in ns. cadence is
        // num/den fps, so the period timebase is den/num seconds; rescale 1 tick
        // of that into ns. Guard against a degenerate cadence by falling back to
        // a single nanosecond (the monotonic guard still applies).
        let frame_period_ns = if cadence.is_valid() && !cadence.is_zero() {
            let period_tb = Rational::new(cadence.den, cadence.num);
            rescale(1, period_tb, ns_timebase()).max(1)
        } else {
            1
        };
        Self {
            wrap,
            timebase,
            cadence,
            frame_period_ns,
            discontinuity_ns: DEFAULT_DISCONTINUITY_NS,
            accumulated_wrap: 0,
            last_raw: None,
            last_raw_ns: None,
            offset: 0,
            last_media_ns: None,
            pending_discontinuity: false,
            mapping: None,
            mapping_generation: Some(0),
        }
    }

    /// Override the discontinuity threshold (nanoseconds). A raw-PTS jump larger
    /// than this triggers a smooth re-anchor.
    #[must_use]
    pub const fn with_discontinuity_ns(mut self, ns: i64) -> Self {
        self.discontinuity_ns = ns;
        self
    }

    /// The declared cadence this normalizer was constructed with.
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// Flag that the *next* frame begins a new timeline segment (e.g. on an
    /// `EXT-X-DISCONTINUITY` tag or a TS discontinuity indicator). The next
    /// [`PtsNormalizer::normalize`] call re-anchors so the output continues
    /// smoothly forward from the last emitted instant.
    pub fn mark_discontinuity(&mut self) {
        self.pending_discontinuity = true;
        self.invalidate_mapping();
    }

    fn invalidate_mapping(&mut self) {
        self.mapping = None;
        self.mapping_generation = self.mapping_generation.and_then(|g| g.checked_add(1));
    }

    /// Current mapping generation, initially zero, even without a raw anchor.
    ///
    /// Advances immediately on explicit discontinuity and on detected raw jumps,
    /// but not on ordinary timestamp wrap or frames. Compare this token after
    /// normalization to retire pending cues from the previous mapping. Tokens
    /// are local to this normalizer; source/reconnect epochs remain caller-owned.
    /// `None` means the counter exhausted: mappings stay unavailable permanently,
    /// and callers must retire pending cues rather than reuse the last token.
    #[must_use]
    pub const fn mapping_generation(&self) -> Option<u64> {
        self.mapping_generation
    }

    /// Copy the latest valid real-PTS mapping for non-mutating event projection.
    ///
    /// Unavailable before a valid raw anchor or after a discontinuity until a
    /// real timestamp reanchors it. Genpts-only frames cannot establish it.
    /// Saved copies belong to the caller's source epoch and must be discarded
    /// when that epoch ends; this normalizer does not manage source lifecycles.
    #[must_use]
    pub const fn mapping_snapshot(&self) -> Option<PtsMappingSnapshot> {
        self.mapping
    }

    /// Normalize one frame's raw timestamp (reclock-to-house: anchor the first
    /// frame to `master_now_ns`).
    ///
    /// `raw` is the input timestamp in the input timebase, or `None` when the
    /// PTS is missing (`AV_NOPTS_VALUE`) — in which case a value is synthesized
    /// from the declared cadence (genpts fallback). `master_now_ns` is the
    /// master monotonic clock reading in nanoseconds, used only to anchor the
    /// very first frame; thereafter the timeline advances by *input* deltas, not
    /// wall-clock, so jitter does not leak into the timeline.
    ///
    /// Returns the rebased, strictly-monotonic [`MediaTime`] for this frame.
    ///
    /// This is the as-built **reclock-to-house** path (ADR-0038's Discard/None
    /// default); it is exactly [`PtsNormalizer::normalize_wallclock`] with no
    /// wall-clock reference.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTimebase`] if the configured input timebase is
    /// degenerate (zero denominator), so timestamp math cannot proceed.
    pub fn normalize(&mut self, raw: Option<i64>, master_now_ns: i64) -> Result<MediaTime> {
        self.normalize_wallclock(raw, master_now_ns, None)
    }

    /// Normalize one frame's raw timestamp, optionally **rebasing onto a common
    /// wall-clock** (ADR-0038's Use path).
    ///
    /// When `wallclock_ref` is `Some` (the source's wall-clock is Trusted **and**
    /// the operator chose `Use`), the **first frame's anchor instant** is the
    /// source's detected wall-clock at that frame's PTS
    /// ([`WallClockRef::wall_at`](multiview_core::wallclock::WallClockRef::wall_at)),
    /// instead of `master_now_ns`. This makes the source's `media_time`
    /// wall-clock-accurate (e.g. HLS `PROGRAM-DATE-TIME`-aligned). When
    /// `wallclock_ref` is `None` (Discard / no detected wall-clock), the anchor is
    /// `master_now_ns` — **byte-identical** to [`PtsNormalizer::normalize`].
    ///
    /// The rebase changes **only the anchor**, never the per-frame delta handling:
    /// the 33-bit/32-bit wrap unwrap, the genpts fallback, the discontinuity
    /// re-anchor, and the strict monotonic guard are all preserved (invariant #3).
    /// The reference's media units must match the input's raw PTS units (e.g. the
    /// HLS 90 kHz media rate against a 90 kHz input timebase).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTimebase`] if the configured input timebase is
    /// degenerate (zero denominator), so timestamp math cannot proceed.
    pub fn normalize_wallclock(
        &mut self,
        raw: Option<i64>,
        master_now_ns: i64,
        wallclock_ref: Option<&multiview_core::wallclock::WallClockRef>,
    ) -> Result<MediaTime> {
        if !self.timebase.is_valid() {
            return Err(Error::InvalidTimebase(
                "input timebase has zero denominator",
            ));
        }

        // (1) genpts fallback: synthesize a raw_ns by continuing the previous
        // emitted instant forward by one frame period. We compute directly in
        // the raw-ns space so discontinuity logic stays consistent.
        let mut mapping_raw = None;
        let raw_ns = if let Some(value) = raw {
            let unwrapped = self.unwrap(value);
            let in_range = self.wrap.modulus().is_none_or(|m| (0..m).contains(&value));
            if in_range
                && value.checked_add(self.accumulated_wrap) == Some(unwrapped)
                && checked_raw_ns(unwrapped, self.timebase).is_some()
            {
                mapping_raw = Some(unwrapped);
            }
            // Rebase the raw value into ns on the input's own (raw) timeline.
            rescale(unwrapped, self.timebase, ns_timebase())
        } else {
            // No PTS: continue from the last raw_ns by one frame period. If there
            // is no prior frame, treat as starting at raw_ns = 0.
            let base = self.last_raw_ns.unwrap_or(0);
            base.saturating_add(self.frame_period_ns)
        };

        // (2) Anchor on the first frame, or re-anchor on a discontinuity / jump.
        let is_first = self.last_raw_ns.is_none();
        let discontinuity = self.pending_discontinuity
            || self
                .last_raw_ns
                .is_some_and(|prev| (raw_ns.saturating_sub(prev)).abs() > self.discontinuity_ns);
        if discontinuity && !self.pending_discontinuity {
            self.invalidate_mapping();
        }
        self.pending_discontinuity = false;

        let media_ns = if is_first {
            // First frame: the anchor instant is the source's detected wall-clock
            // (Use path) when a ref is available AND this frame carries a real PTS;
            // otherwise master_now (the as-built reclock-to-house default). The ref
            // maps the frame's PTS (in the ref's media units) to its wall-clock
            // instant. offset = anchor - raw_ns, so future deltas ride off the
            // anchor exactly as in the house path.
            let anchor_ns = match (wallclock_ref, raw) {
                (Some(wc), Some(pts)) => wc.wall_at(pts),
                // No ref, or a genpts first frame (no PTS to map): house anchor.
                _ => master_now_ns,
            };
            self.offset = anchor_ns.saturating_sub(raw_ns);
            anchor_ns
        } else if discontinuity {
            // Re-anchor: continue from the last emitted instant by one frame
            // period, and recompute offset so future deltas are relative to the
            // NEW raw value. This keeps the output smoothly forward.
            let continue_at = self
                .last_media_ns
                .unwrap_or(master_now_ns)
                .saturating_add(self.frame_period_ns);
            self.offset = continue_at.saturating_sub(raw_ns);
            continue_at
        } else {
            raw_ns.saturating_add(self.offset)
        };

        // (3) Monotonic guard: never emit a non-increasing timestamp.
        let guarded = match self.last_media_ns {
            Some(prev) if media_ns <= prev => prev.saturating_add(1),
            _ => media_ns,
        };

        if discontinuity {
            self.mapping = None;
        }
        if raw.is_some() {
            self.mapping = mapping_raw
                .zip(self.mapping_generation)
                .map(|(raw, generation)| PtsMappingSnapshot {
                    raw,
                    timebase: self.timebase,
                    media_ns: guarded,
                    generation,
                });
        }
        self.last_raw_ns = Some(raw_ns);
        self.last_media_ns = Some(guarded);
        Ok(MediaTime::from_nanos(guarded))
    }

    /// Delta-based wrap unwrap: accumulate a 64-bit counter from masked raw
    /// values. A negative delta whose magnitude exceeds half the modulus is a
    /// forward wrap; a positive delta exceeding half the modulus is a backward
    /// wrap (rare, but symmetric for robustness).
    fn unwrap(&mut self, raw: i64) -> i64 {
        let (Some(modulus), Some(half)) = (self.wrap.modulus(), self.wrap.half_modulus()) else {
            // No wrapping configured: pass the value straight through.
            self.last_raw = Some(raw);
            return raw;
        };
        if let Some(last) = self.last_raw {
            let delta = raw.saturating_sub(last);
            if delta < -half {
                self.accumulated_wrap = self.accumulated_wrap.saturating_add(modulus);
            } else if delta > half {
                self.accumulated_wrap = self.accumulated_wrap.saturating_sub(modulus);
            }
        }
        self.last_raw = Some(raw);
        raw.saturating_add(self.accumulated_wrap)
    }
}

#[cfg(test)]
mod mapping_generation_tests {
    use super::{PtsNormalizer, Rational, WrapBits};

    #[test]
    fn generation_exhaustion_permanently_disables_snapshots() {
        let mut n = PtsNormalizer::new(WrapBits::None, Rational::new(1, 90_000), Rational::FPS_25);
        n.mapping_generation = Some(u64::MAX);
        assert!(n.normalize(Some(0), 0).is_ok());
        assert!(n.mapping_snapshot().is_some());
        n.mark_discontinuity();
        assert_eq!(n.mapping_generation(), None);
        assert!(n.mapping_snapshot().is_none());
        assert!(n.normalize(Some(1), 0).is_ok());
        assert!(n.mapping_snapshot().is_none());
        assert!(n.normalize(Some(90_000 * 60), 0).is_ok());
        n.mark_discontinuity();
        assert_eq!(n.mapping_generation(), None);
        assert!(n.mapping_snapshot().is_none());
    }
}
