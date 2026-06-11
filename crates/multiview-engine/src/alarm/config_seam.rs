//! The **config → analyser seam**: pure mappers that turn an operator-authored
//! config [`Probe`](multiview_config::probe::Probe) declaration into the engine
//! analyser configuration ([`BlackConfig`]/[`FreezeConfig`]) it drives, threading
//! the operator's **threshold and detection zone** into the analyser instead of a
//! hardcoded default (ADR-MV001, M10).
//!
//! These mappers are the bridge consumed by the run's per-tick fault detector
//! (the CLI `FaultDetector`): it pairs them with
//! [`crate::alarm::state::AlarmStateMachine::from_probe`] — which builds the X.733
//! *lifecycle* (severity + dwell + latch + scope) from the same declaration — so a
//! declared probe's analyser policy and its alarm lifecycle both come from config,
//! not from a hardcoded default.
//!
//! Everything here is a pure value mapping: no clocks, channels, sleeps or I/O, so
//! it cannot touch the output clock or back-pressure the engine (invariants #1 +
//! #10). A config zone that somehow fails the engine's stricter constructor (it
//! should not for a validated probe) degrades to the full frame rather than
//! panicking.
use multiview_config::probe::{DetectionZone as ConfigZone, ProbeKind};

use crate::probe::{BlackConfig, DetectionZone, FreezeConfig};

/// Map a config [`DetectionZone`](ConfigZone) (validated `0.0..=1.0` fractions) to
/// the engine [`DetectionZone`].
///
/// A config zone that somehow fails the engine's stricter constructor (it
/// should not for a validated probe) falls back to the full frame, so a
/// malformed zone degrades to "analyse the whole picture" rather than panicking
/// on the control tick.
#[must_use]
pub fn engine_zone(zone: ConfigZone) -> DetectionZone {
    DetectionZone::new(zone.x, zone.y, zone.w, zone.h).unwrap_or(DetectionZone::FULL)
}

/// Build the engine **black** analyser config from a config [`ProbeKind`],
/// threading the operator-authored `luma_threshold` (an 8-bit ceiling) and
/// detection zone into the engine analyser (the config→analyser seam — **not** a
/// hardcoded default). Returns [`None`] for a non-black kind.
///
/// The config threshold is on the same `0.0..=255.0` mean-luma scale the engine
/// analyser compares against, so the widen is exact (`f64::from`, no `as` cast).
#[must_use]
pub fn black_config_from_kind(kind: &ProbeKind) -> Option<BlackConfig> {
    match *kind {
        ProbeKind::Black {
            luma_threshold,
            zone,
        } => Some(BlackConfig {
            luma_threshold: f64::from(luma_threshold),
            zone: engine_zone(zone),
        }),
        _ => None,
    }
}

/// Build the engine **freeze** analyser config from a config [`ProbeKind`],
/// threading the operator-authored `difference_threshold` (a per-mille,
/// `0..=1000`) and detection zone into the engine analyser. Returns [`None`] for a
/// non-freeze kind.
///
/// The per-mille threshold maps to the engine's `0.0..=1.0` changed-sample
/// fraction by dividing by 1000 exactly (`f64::from`, no `as` cast). The remaining
/// engine knobs (the per-sample `diff_tolerance`) keep their defaults.
#[must_use]
pub fn freeze_config_from_kind(kind: &ProbeKind) -> Option<FreezeConfig> {
    match *kind {
        ProbeKind::Freeze {
            difference_threshold,
            zone,
        } => Some(FreezeConfig {
            change_threshold: f64::from(difference_threshold) / 1000.0,
            zone: engine_zone(zone),
            ..FreezeConfig::default()
        }),
        _ => None,
    }
}
