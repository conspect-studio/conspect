//! HLS / wall-clock input pacer (invariant #4).
//!
//! On connect or reconnect, several already-published HLS segments sit on the
//! origin; a naive reader pulls them all back-to-back, time-warping the tile and
//! blowing up memory. **`-re` is for files, not live ingest** — it does not
//! smooth the connect burst and refills via an unthrottled burst after a stall
//! (see [ADR-T004]). Instead, this pacer schedules each frame's release against
//! the wall clock:
//!
//! ```text
//! on first frame: anchor_wall = now(); pts0 = pts
//! release frame when now() >= anchor_wall + (pts - pts0)
//! re-anchor (pts0, anchor_wall) on EXT-X-DISCONTINUITY or a large PTS jump
//! bounded catch-up (<= ~1.25x) for small drift, never an instant seek
//! ```
//!
//! The pacer is **clock-injected**: callers pass the current wall-clock reading
//! (`now_ns`) explicitly, so pacing decisions are pure functions of `(pts,
//! now)` and deterministically testable without sleeping.
//!
//! [ADR-T004]: HLS ingest pacing.
use crate::error::{Error, Result};
use multiview_core::time::MediaTime;

/// Default discontinuity threshold for the pacer (nanoseconds). A PTS jump
/// larger than this re-anchors instead of scheduling a far-future release.
const DEFAULT_DISCONTINUITY_NS: i64 = 10_000_000_000;

/// Pacer tuning.
///
/// The catch-up rate is an exact rational `num/den` (default `5/4` = 1.25x): the
/// maximum factor by which releases may be advanced to recover small drift,
/// never an instant seek. `den` must be non-zero and `num >= den` (a rate below
/// 1.0 would slow releases, which the pacer never does).
///
/// A plain configuration record: callers construct it with all fields (or via
/// [`PacerConfig::default`]). It is intentionally *not* `#[non_exhaustive]` so
/// callers and tests can build it directly with `..PacerConfig::default()`.
#[derive(Debug, Clone, Copy)]
pub struct PacerConfig {
    /// Numerator of the maximum catch-up rate.
    pub max_catchup_rate_num: i64,
    /// Denominator of the maximum catch-up rate.
    pub max_catchup_rate_den: i64,
    /// PTS-jump threshold (ns) beyond which the pacer re-anchors.
    pub discontinuity_ns: i64,
}

impl Default for PacerConfig {
    fn default() -> Self {
        Self {
            max_catchup_rate_num: 5,
            max_catchup_rate_den: 4,
            discontinuity_ns: DEFAULT_DISCONTINUITY_NS,
        }
    }
}

/// A pacing decision for a submitted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Release {
    /// The frame is due now (or overdue) and should be released immediately.
    Now,
    /// The frame is not yet due; release it when the wall clock reaches the
    /// given nanosecond instant.
    At(i64),
}

/// Wall-clock input pacer with an injected clock.
///
/// Anchor by submitting the first frame (which always returns [`Release::Now`]);
/// thereafter [`Pacer::submit`] returns the wall-clock instant at which each
/// frame becomes due. Signal a timeline break with [`Pacer::mark_discontinuity`].
#[derive(Debug)]
pub struct Pacer {
    config: PacerConfig,
    /// `(anchor_wall_ns, pts0_ns)` set on the anchoring frame.
    anchor: Option<(i64, i64)>,
    /// The previous submitted PTS, for jump detection.
    last_pts: Option<i64>,
    /// Whether a discontinuity has been flagged for the next frame.
    pending_discontinuity: bool,
}

impl Pacer {
    /// Construct a pacer with the given configuration.
    #[must_use]
    pub fn new(config: PacerConfig) -> Self {
        Self {
            config,
            anchor: None,
            last_pts: None,
            pending_discontinuity: false,
        }
    }

    /// Whether the pacer has seen its anchoring frame.
    #[must_use]
    pub const fn is_anchored(&self) -> bool {
        self.anchor.is_some()
    }

    /// Flag that the next submitted frame begins a new timeline segment
    /// (`EXT-X-DISCONTINUITY` or an inferred timeline break). The next
    /// [`Pacer::submit`] re-anchors `(anchor_wall, pts0)` to that frame.
    pub fn mark_discontinuity(&mut self) {
        self.pending_discontinuity = true;
    }

    /// Submit a frame with presentation timestamp `pts`, given the current
    /// wall-clock reading `now_ns`.
    ///
    /// Anchors on the first frame (and re-anchors on a flagged or inferred
    /// discontinuity), returning [`Release::Now`]. Otherwise returns
    /// [`Release::Now`] if the frame is due, or [`Release::At`] with the future
    /// wall-clock instant at which it becomes due.
    pub fn submit(&mut self, pts: MediaTime, now_ns: i64) -> Release {
        let pts_ns = pts.as_nanos();
        let discontinuity = self.pending_discontinuity
            || self.last_pts.is_some_and(|prev| {
                pts_ns.saturating_sub(prev).abs() > self.config.discontinuity_ns
            });
        self.pending_discontinuity = false;

        if self.anchor.is_none() || discontinuity {
            // Anchor / re-anchor to this frame at the current wall time.
            self.anchor = Some((now_ns, pts_ns));
            self.last_pts = Some(pts_ns);
            return Release::Now;
        }

        self.last_pts = Some(pts_ns);
        // Safe: `anchor` is Some here (checked above).
        let deadline = self.deadline_for(pts_ns).unwrap_or(now_ns);
        if now_ns >= deadline {
            Release::Now
        } else {
            Release::At(deadline)
        }
    }

    /// The wall-clock release deadline (ns) for a frame at `pts`, or an error if
    /// the pacer is not yet anchored.
    ///
    /// `deadline = anchor_wall + (pts - pts0)`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PacerNotAnchored`] if no anchoring frame has been
    /// submitted yet.
    pub fn release_deadline(&self, pts: MediaTime) -> Result<i64> {
        self.deadline_for(pts.as_nanos())
            .ok_or(Error::PacerNotAnchored)
    }

    /// The catch-up release deadline (ns) for a frame at `pts`: the nominal
    /// deadline with the wall-clock interval shrunk by the configured catch-up
    /// rate, so the pacer can recover small drift by advancing releases (up to
    /// the rate cap) — never an instant seek.
    ///
    /// If the pacer is not yet anchored, this returns `pts`'s own nanosecond
    /// value as a degenerate-but-safe fallback (the caller will anchor on submit).
    #[must_use]
    pub fn release_deadline_catchup(&self, pts: MediaTime) -> i64 {
        let Some((anchor_wall, pts0)) = self.anchor else {
            return pts.as_nanos();
        };
        let nominal_interval = pts.as_nanos().saturating_sub(pts0);
        if nominal_interval <= 0 {
            return anchor_wall.saturating_add(nominal_interval);
        }
        // Effective interval = nominal * den / num (rate >= 1 shrinks it). Guard
        // against a degenerate or sub-1.0 rate by falling back to the nominal.
        let num = self.config.max_catchup_rate_num;
        let den = self.config.max_catchup_rate_den;
        let shrunk = if num >= den && den > 0 {
            // i128 to avoid overflow on the multiply.
            let scaled =
                i128::from(nominal_interval).saturating_mul(i128::from(den)) / i128::from(num);
            i64::try_from(scaled).unwrap_or(nominal_interval)
        } else {
            nominal_interval
        };
        anchor_wall.saturating_add(shrunk)
    }

    /// Compute `anchor_wall + (pts - pts0)` if anchored.
    fn deadline_for(&self, pts_ns: i64) -> Option<i64> {
        let (anchor_wall, pts0) = self.anchor?;
        Some(anchor_wall.saturating_add(pts_ns.saturating_sub(pts0)))
    }
}
