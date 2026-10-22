//! Per-source **wall-clock trust** + the media-PTS→wall-clock affine map
//! (ADR-0038, SYNC-0).
//!
//! Multiview ingests many sources whose absolute time-of-day relationship is
//! operationally important. A source may carry a wall-clock standard (HLS
//! `EXT-X-PROGRAM-DATE-TIME`, RTCP Sender Report, PTP, …) mapping its media PTS to
//! an absolute instant — or it may carry **none** (plain RTSP without SR, HLS
//! without PDT, SRT/RTMP/file, synthetic sources). This module owns the **shared
//! types** that:
//!
//! 1. **honestly classify** a source's wall-clock trust ([`WallClockTier`]) and
//!    record *where* the assertion came from ([`WallClockOrigin`]) — the tier is
//!    **measured at runtime, never authored**;
//! 2. carry the operator's per-source verb ([`WallClockChoice`] — `Use`/`Discard`);
//! 3. express the media→wall affine map ([`WallClockRef`]) used on the **Use**
//!    path to rebase a source onto a common wall-clock timeline.
//!
//! ## The honest content-sync rule
//!
//! A **house-clocked** source (Discard / no detected wall-clock) is aligned by
//! **arrival**, never by the real capture instant — the capture-time information
//! was thrown away. So a house-clocked source **cannot** be truly content-synced:
//! it is "house-clocked / live" (lowest latency, roughly arrival-aligned, never
//! frame-accurate to the event). True multi-cam content-sync requires a
//! **Trusted** common-timeline wall-clock. [`SyncMode`] encodes this in the type
//! system: only [`SyncMode::ContentSynced`] carries a [`WallClockRef`];
//! [`SyncMode::HouseClocked`] carries none.
//!
//! All arithmetic is **exact integer nanoseconds / exact rationals** (invariant
//! #3) — never float fps, which drifts (~3.6 s/hour for the NTSC `1001` family).
//! Enum serde is internally/externally **tagged** (snake-case string tags) so the
//! types are robust across TOML and JSON — **never** `untagged`.
use serde::{Deserialize, Serialize};

use crate::time::{rescale, Rational};

/// How well a source's detected wall-clock is trusted (ADR-0038 §1).
///
/// Mirrors the engine lock-state lifecycle (`Locked`→Trusted, `Holdover`→Suspected,
/// `Freerun`/`Acquiring`→None): the same trust boundary, surfaced per source. The
/// only tier that can enter [`SyncMode::ContentSynced`] is [`WallClockTier::Trusted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WallClockTier {
    /// The wall-clock is present, plausible, and fresh — frame/ms-accurate per its
    /// tier. The only tier that can content-sync.
    Trusted,
    /// The wall-clock is present but jittery / out-of-tolerance / asserts an
    /// implausible jump — it falls back to reclock-to-house. A bare timecode
    /// (SMPTE SEI / RP188) caps here: it is a label, not a disciplined wall-clock.
    Suspected,
    /// No wall-clock was detected (absent or stale). Reclock-to-house only.
    None,
}

impl WallClockTier {
    /// A short, lower-case textual label (accessibility — read without colour).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Suspected => "suspected",
            Self::None => "none",
        }
    }

    /// Whether this tier may drive content-sync. Only [`WallClockTier::Trusted`]
    /// qualifies; everything else is house-clocked.
    #[must_use]
    pub const fn can_content_sync(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

/// Which transport standard supplied the wall-clock assertion (ADR-0038 §0).
///
/// Name-aligned with the overlay timecode sources, extended with the wall-clock
/// carriers. [`WallClockOrigin::None`] means no standard was present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WallClockOrigin {
    /// PTP / IEEE 1588 / SMPTE ST 2059 grandmaster timeline (strongest; the only
    /// phase-accurate origin).
    Ptp,
    /// RTCP Sender Report NTP↔RTP pair (RFC 3550 §6.4.1).
    RtcpSr,
    /// HLS `EXT-X-PROGRAM-DATE-TIME` (RFC 8216 §4.3.2.6).
    ProgramDateTime,
    /// MPEG-TS TEMI adaptation-field affine timeline (ISO/IEC 13818-1).
    Temi,
    /// DVB TDT/TOT network service-of-day UTC (no PTS bind — a coarse house hint).
    DvbTdtTot,
    /// SMPTE ST 12-1 SEI timecode (a per-frame label, no inherent UTC anchor —
    /// caps at [`WallClockTier::Suspected`]).
    SmpteSeiTimecode,
    /// RP188 / ATC ancillary SMPTE timecode (a label; Trusted only joined to PTP).
    Rp188Atc,
    /// No wall-clock standard was present.
    #[default]
    None,
}

impl WallClockOrigin {
    /// A short, lower-case textual label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ptp => "ptp",
            Self::RtcpSr => "rtcp_sr",
            Self::ProgramDateTime => "program_date_time",
            Self::Temi => "temi",
            Self::DvbTdtTot => "dvb_tdt_tot",
            Self::SmpteSeiTimecode => "smpte_sei_timecode",
            Self::Rp188Atc => "rp188_atc",
            Self::None => "none",
        }
    }

    /// Whether this origin is a **bare timecode label** (no inherent UTC anchor),
    /// which can never exceed [`WallClockTier::Suspected`]. SMPTE SEI and RP188/ATC
    /// are labels about the source, not disciplined wall-clocks (ADR-0038 §1).
    #[must_use]
    pub const fn is_bare_timecode(self) -> bool {
        matches!(self, Self::SmpteSeiTimecode | Self::Rp188Atc)
    }
}

/// The operator's per-source wall-clock verb (ADR-0038 §1).
///
/// Config carries **only** this verb; the [`WallClockTier`] is measured at runtime.
/// On `Use` + a detected [`WallClockRef`], the source is rebased onto the common
/// wall-clock timeline; on `Discard` (or no ref), the as-built reclock-to-house
/// path is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WallClockChoice {
    /// Use the detected wall-clock (rebase onto the common timeline when a
    /// [`WallClockRef`] is available). The default.
    #[default]
    Use,
    /// Discard the detected wall-clock; keep the as-built reclock-to-house anchor.
    Discard,
}

/// The per-source wall-clock trust result, surfaced to the API/UI (ADR-0038 §1).
///
/// The `tier` and `origin` are **runtime measurements**; `choice` is the
/// operator's authored verb. This is a closed value record callers construct
/// directly; the field-level enums carry `#[non_exhaustive]` so new tiers/origins
/// can be added without breaking this struct's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WallClockTrust {
    /// The runtime-measured trust tier.
    pub tier: WallClockTier,
    /// Which standard supplied the wall-clock (or [`WallClockOrigin::None`]).
    pub origin: WallClockOrigin,
    /// The operator's Use/Discard verb.
    pub choice: WallClockChoice,
}

impl WallClockTrust {
    /// A source with no detected wall-clock: tier None, origin None, the default
    /// verb. The honest baseline for a source that carries no standard.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            tier: WallClockTier::None,
            origin: WallClockOrigin::None,
            choice: WallClockChoice::Use,
        }
    }

    /// Whether this trust **effectively** drives content-sync: the operator chose
    /// `Use` **and** the measured tier can content-sync. A `Discard`, or a tier
    /// below Trusted, is house-clocked regardless.
    #[must_use]
    pub const fn is_effectively_content_synced(self) -> bool {
        matches!(self.choice, WallClockChoice::Use) && self.tier.can_content_sync()
    }
}

/// The media-PTS→wall-clock **affine map** (ADR-0038 §1).
///
/// `wall(pts) = wall_at_anchor_ns + rescale(pts − media_at_anchor, rate)`, where
/// `pts` and `media_at_anchor` are measured in units of `rate` (e.g. 90 kHz ticks),
/// and `rate` is the media timebase as **frames/ticks per second** (`num/den`).
/// All arithmetic is exact (`i128` intermediates via [`rescale`]) — never float.
///
/// Built from whatever standard the transport carries: e.g. for HLS, the
/// `EXT-X-PROGRAM-DATE-TIME` instant of a segment paired with that segment's first
/// sample's media PTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WallClockRef {
    /// The wall-clock instant (integer ns past the Unix epoch) at the anchor
    /// sample.
    pub wall_at_anchor_ns: i64,
    /// The media PTS of the anchor sample, in units of `rate`.
    pub media_at_anchor: i64,
    /// The media rate (ticks/frames per second) as an exact rational, e.g.
    /// `90000/1` for a 90 kHz media timebase.
    pub rate: Rational,
}

impl WallClockRef {
    /// Construct an affine map binding `media_at_anchor` (in `rate` units) to
    /// `wall_at_anchor_ns` (integer ns past the Unix epoch).
    #[must_use]
    pub const fn new(wall_at_anchor_ns: i64, media_at_anchor: i64, rate: Rational) -> Self {
        Self {
            wall_at_anchor_ns,
            media_at_anchor,
            rate,
        }
    }

    /// Map a media PTS (in `rate` units) to its wall-clock instant (integer ns past
    /// the Unix epoch): `wall(pts) = wall_at_anchor + rescale(pts − media_anchor)`.
    ///
    /// The delta `pts − media_anchor` is rescaled from the media `rate` into
    /// nanoseconds exactly. A degenerate `rate` (zero denominator) yields a zero
    /// delta (the anchor instant) rather than panicking — callers should validate
    /// the rate with [`Rational::is_valid`].
    #[must_use]
    pub fn wall_at(self, pts: i64) -> i64 {
        let delta_ticks = pts.saturating_sub(self.media_at_anchor);
        // rate is ticks-per-second, so a tick spans 1/rate seconds, i.e. a timebase
        // of rate.den/rate.num seconds per tick. Rescale that delta into ns.
        let tick_timebase = Rational::new(self.rate.den, self.rate.num);
        let delta_ns = rescale(delta_ticks, tick_timebase, Rational::new(1, 1_000_000_000));
        self.wall_at_anchor_ns.saturating_add(delta_ns)
    }

    /// The **inverse** map (ADR-M010, the outbound presentation epoch): a
    /// wall-clock instant (integer ns past the Unix epoch) back to the media PTS
    /// (in `rate` units) the affine map associates with it:
    /// `media(wall) = media_anchor + rescale(wall − wall_anchor, ns → rate)`.
    ///
    /// Exact integer arithmetic (`i128` intermediates via [`rescale`], rounding
    /// half away from zero) — never float. For the canonical 1 GHz (nanosecond)
    /// epoch rate the round trip `media_at(wall_at(pts)) == pts` is exact; for a
    /// coarser rate (e.g. 90 kHz) it is exact to within one tick (the
    /// quantisation of [`WallClockRef::wall_at`]). A degenerate `rate` yields a
    /// zero delta (the anchor media position) rather than panicking.
    #[must_use]
    pub fn media_at(self, wall_ns: i64) -> i64 {
        let delta_ns = wall_ns.saturating_sub(self.wall_at_anchor_ns);
        // Rescale the ns delta into media ticks: from a 1 ns timebase into the
        // rate.den/rate.num seconds-per-tick timebase.
        let tick_timebase = Rational::new(self.rate.den, self.rate.num);
        let delta_ticks = rescale(delta_ns, Rational::new(1, 1_000_000_000), tick_timebase);
        self.media_at_anchor.saturating_add(delta_ticks)
    }
}

/// The effective per-source sync mode (ADR-0038 §1), surfaced so the UI shows which
/// mode each tile is in.
///
/// The type system encodes the honest rule: only [`SyncMode::ContentSynced`]
/// carries a [`WallClockRef`]; a [`SyncMode::HouseClocked`] source has no
/// wall-clock map and is aligned by arrival (never frame-accurate to the event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyncMode {
    /// The source is rebased onto a common wall-clock timeline via its detected
    /// affine map (Trusted + Used).
    ContentSynced(WallClockRef),
    /// The source is house-clocked: anchored by arrival, advancing by its own PTS
    /// deltas. Cannot be content-synced.
    HouseClocked,
}

impl SyncMode {
    /// The wall-clock map when content-synced, or `None` when house-clocked.
    #[must_use]
    pub const fn wallclock_ref(&self) -> Option<&WallClockRef> {
        match self {
            Self::ContentSynced(wc) => Some(wc),
            Self::HouseClocked => None,
        }
    }

    /// Whether this mode is content-synced.
    #[must_use]
    pub const fn is_content_synced(&self) -> bool {
        matches!(self, Self::ContentSynced(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_timecode_origins_are_flagged() {
        assert!(WallClockOrigin::SmpteSeiTimecode.is_bare_timecode());
        assert!(WallClockOrigin::Rp188Atc.is_bare_timecode());
        assert!(!WallClockOrigin::ProgramDateTime.is_bare_timecode());
        assert!(!WallClockOrigin::Ptp.is_bare_timecode());
    }

    #[test]
    fn only_use_plus_trusted_is_effectively_content_synced() {
        let used_trusted = WallClockTrust {
            tier: WallClockTier::Trusted,
            origin: WallClockOrigin::ProgramDateTime,
            choice: WallClockChoice::Use,
        };
        assert!(used_trusted.is_effectively_content_synced());

        let discarded = WallClockTrust {
            choice: WallClockChoice::Discard,
            ..used_trusted
        };
        assert!(!discarded.is_effectively_content_synced());

        let suspected = WallClockTrust {
            tier: WallClockTier::Suspected,
            ..used_trusted
        };
        assert!(!suspected.is_effectively_content_synced());
    }

    #[test]
    fn none_trust_is_the_honest_baseline() {
        let n = WallClockTrust::none();
        assert_eq!(n.tier, WallClockTier::None);
        assert_eq!(n.origin, WallClockOrigin::None);
        assert!(!n.is_effectively_content_synced());
    }
}
