//! The `[timing]` block (ADR-M010): outbound presentation-timing knobs.
//!
//! One outbound `WallClockRef` epoch is published per program (DEV-C1); this
//! block carries the two deployment-level knobs that go with it:
//!
//! * **`link_offset_ms`** — the fixed receiver-side presentation delay every
//!   epoch consumer adds to `wall_at(pts)` before presenting (AES67's
//!   link-offset rule applied to video). The default ≈ 2× max network jitter +
//!   decode time; **uniformity across nodes is the goal, not smallness** — a
//!   uniform 150 ms beats a per-node-minimised spread.
//! * **`ptp_phc`** — the optional PTP Hardware Clock device path (e.g.
//!   `/dev/ptp0`) the `ptp`-feature build samples to discipline the epoch's
//!   wall estimate (ADR-T012: PTP outranks the system clock while
//!   disciplined). A build **without** the `ptp` feature fails the run at
//!   startup when this is set (`multiview-cli`'s timing gate — the same
//!   fail-fast contract as a `display` output in a non-`display-kms` build).
//!   The PHC **never paces the output clock** (invariant #1): it disciplines
//!   a published estimate only.
//! * **`ptp_utc_offset_s`** — the **timescale conversion** for the PTP leg.
//!   Under standard linuxptp (ptp4l on the SMPTE ST 2059-2 profile +
//!   phc2sys, the ADR-T012 deployment) the PHC carries **PTP time = TAI**,
//!   while the published epoch — and every surface stamped from it (HLS
//!   `EXT-X-PROGRAM-DATE-TIME`, the RTCP SR NTP word) — is **UTC**. This
//!   integer-second offset (current TAI−UTC = **37**; sourced from ptp4l's
//!   `currentUtcOffset` in deployment) is subtracted from the PHC-derived
//!   estimate so the epoch is always UTC. A deployment whose PHC genuinely
//!   carries UTC sets `0`. Exact integer seconds→ns; never float.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The inclusive upper bound for [`TimingConfig::link_offset_ms`]: beyond 10 s
/// the value is a typo, not a presentation-delay policy (the same bound
/// rationale as the sync-group member-offset cap).
pub const MAX_LINK_OFFSET_MS: u32 = 10_000;

/// The inclusive upper bound for [`TimingConfig::ptp_utc_offset_s`]: TAI−UTC
/// is 37 s today and grows by leap seconds — anywhere near 1000 s (or any
/// negative value) is a typo/sign error, not timescale policy.
pub const MAX_PTP_UTC_OFFSET_S: i64 = 1_000;

/// `[timing]` — the outbound presentation-timing knobs (ADR-M010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TimingConfig {
    /// The fixed receiver-side presentation delay in milliseconds, added by
    /// every epoch consumer to `wall_at(pts)` (AES67 link-offset semantics
    /// applied to video; uniformity over smallness). Default **150 ms**.
    #[serde(default = "default_link_offset_ms")]
    pub link_offset_ms: u32,
    /// Optional PTP Hardware Clock device path (e.g. `/dev/ptp0`) sampled by
    /// the `ptp`-feature build to discipline the epoch's wall estimate.
    /// `None` ⇒ the chrony/NTP-disciplined system clock is the wall source.
    /// Setting this in a build without the `ptp` feature fails the run at
    /// startup (fail-fast; never a silent downgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptp_phc: Option<String>,
    /// The PTP-leg timescale conversion in **integer seconds**: the PHC under
    /// standard linuxptp carries TAI, the published epoch is UTC, and this
    /// offset (current TAI−UTC = 37; ptp4l's `currentUtcOffset`) is
    /// subtracted when deriving the wall estimate from the PHC. Set `0` for a
    /// (nonstandard) PHC that carries UTC. Ignored while the system clock is
    /// the selected reference.
    #[serde(default = "default_ptp_utc_offset_s")]
    pub ptp_utc_offset_s: i64,
}

/// The default outbound link offset (ms): inside ADR-M010's typical
/// 100–300 ms envelope.
const fn default_link_offset_ms() -> u32 {
    150
}

/// The default PTP timescale conversion: the current TAI−UTC offset (37 s
/// since 2017-01-01; in deployment confirm against ptp4l's `currentUtcOffset`).
const fn default_ptp_utc_offset_s() -> i64 {
    37
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            link_offset_ms: default_link_offset_ms(),
            ptp_phc: None,
            ptp_utc_offset_s: default_ptp_utc_offset_s(),
        }
    }
}

impl TimingConfig {
    /// The link offset in integer nanoseconds (exact: `ms × 1_000_000`).
    #[must_use]
    pub fn link_offset_ns(&self) -> i64 {
        i64::from(self.link_offset_ms).saturating_mul(1_000_000)
    }

    /// The PTP timescale conversion in integer nanoseconds (exact:
    /// `s × 1_000_000_000`).
    #[must_use]
    pub fn ptp_utc_offset_ns(&self) -> i64 {
        self.ptp_utc_offset_s.saturating_mul(1_000_000_000)
    }

    /// Validate the block: the link offset and the PTP timescale conversion
    /// must be within their sane bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when `link_offset_ms` exceeds
    /// [`MAX_LINK_OFFSET_MS`], or when `ptp_utc_offset_s` is negative or
    /// exceeds [`MAX_PTP_UTC_OFFSET_S`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.link_offset_ms > MAX_LINK_OFFSET_MS {
            return Err(ConfigError::Validation(format!(
                "timing: link_offset_ms ({}) exceeds the {MAX_LINK_OFFSET_MS} ms bound — \
                 beyond it the value is a typo, not a presentation-delay policy",
                self.link_offset_ms
            )));
        }
        if !(0..=MAX_PTP_UTC_OFFSET_S).contains(&self.ptp_utc_offset_s) {
            return Err(ConfigError::Validation(format!(
                "timing: ptp_utc_offset_s ({}) is outside 0..={MAX_PTP_UTC_OFFSET_S} — \
                 TAI−UTC is 37 s today (ptp4l currentUtcOffset); an out-of-band value \
                 is a typo or sign error, not timescale policy",
                self.ptp_utc_offset_s
            )));
        }
        Ok(())
    }
}
