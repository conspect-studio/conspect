//! Pure source-event presentation adapter shared by compositor and web adapters.
//!
//! This module renders nothing and samples no clock. Owners retain a monotonic
//! lifecycle-entry timestamp per occurrence, updating it only on transitions
//! (not on each projection/revision). In particular, stale/unknown clocks share
//! one uncertain interval. Keep that timestamp after expiry so polling cannot
//! resurrect a cleared badge. Media time alone drives the timer; observation
//! time alone drives terminal/uncertain expiry. No synthetic sources are made.
use serde::{Deserialize, Serialize};

use crate::source_event::{
    EventBoundary, EventLifecycle, EventOrigin, MarkerDescriptor, PresentationClock, SourceEvent,
    SourceEventKey,
};

/// Saved selection, independent of a tile/strip renderer or generated event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOverlayBinding {
    /// Required source selection.
    pub source_id: String,
    /// Optional occurrence selection; `None` permits any occurrence.
    pub event_id: Option<String>,
    /// Optional evidence origin selection.
    pub origin: Option<EventOrigin>,
    /// Display lifecycle policy in exact monotonic nanoseconds.
    pub policy: EventOverlayPolicy,
}

/// Explicit display durations. Zero hold clears immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOverlayPolicy {
    /// How far before source start an announced event becomes visible.
    pub warning_lead_ns: u64,
    /// How long to retain a confirmed end badge after observing the transition.
    pub terminal_hold_ns: u64,
    /// How long to retain uncertainty, including expected-end overrun.
    pub uncertain_hold_ns: u64,
}

/// Caller-owned monotonic timing, never wall-clock or source media time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayObservationTime {
    /// Current monotonic observation nanoseconds.
    pub now_ns: u64,
    /// First observation of this lifecycle for this occurrence.
    pub lifecycle_since_ns: u64,
}

/// Compact semantic label tokens; adapters choose localized text and graphics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayLabel {
    /// Source start is approaching.
    Upcoming,
    /// Explicit start is active.
    Active,
    /// Explicit end has been presented.
    Ended,
    /// Timing or boundary evidence is insufficient.
    Uncertain,
    /// Expected duration elapsed without a confirmed end.
    Overrun,
    /// Presence-only evidence, not an aired advertisement claim.
    Observed,
}

/// Timer direction/meaning, separate from localized formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayTimerKind {
    /// Remaining time to the explicit start.
    UntilStart,
    /// Remaining time to a confirmed or expected end.
    Remaining,
    /// Time since explicit start when no end is known.
    Elapsed,
    /// Time beyond the expected end; does not confirm an end.
    Overrun,
}

/// Exact nonnegative duration suitable for JSON consumers without float loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayTimer {
    /// Semantic interpretation of the duration.
    pub kind: OverlayTimerKind,
    /// Base-10 integer nanoseconds, up to `u64::MAX`; deliberately not a JSON
    /// number (JavaScript numbers cannot represent every nanosecond).
    pub nanoseconds: String,
}

/// Wire-safe occurrence identity. Epochs are canonical decimal strings rather
/// than JSON numbers, preserving identity in browser consumers above 2^53.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayEventKey {
    /// Source identity.
    pub source_id: String,
    /// Exact base-10 unsigned epoch identity.
    pub epoch: String,
    /// Distinct evidence origin.
    pub origin: EventOrigin,
    /// Occurrence identity, never reusable asset identity.
    pub event_id: String,
}

impl From<&SourceEventKey> for OverlayEventKey {
    fn from(key: &SourceEventKey) -> Self {
        Self {
            source_id: key.source_id.clone(),
            epoch: key.epoch.to_string(),
            origin: key.origin.clone(),
            event_id: key.event_id.clone(),
        }
    }
}

/// Observational badge/timer data, not a render or automation command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOverlay {
    /// Full occurrence identity so consumers never merge evidence origins.
    pub key: OverlayEventKey,
    /// Underlying lifecycle; overrun remains uncertain, not ended.
    pub lifecycle: EventLifecycle,
    /// Label token for compact display.
    pub label: OverlayLabel,
    /// Absent whenever source timing cannot support a timer.
    pub timer: Option<OverlayTimer>,
    /// Precomputed marker identity text, when the evidence carried one.
    pub descriptor: Option<MarkerDescriptor>,
}

fn timer(kind: OverlayTimerKind, a: i64, b: i64) -> OverlayTimer {
    OverlayTimer {
        kind,
        nanoseconds: a.abs_diff(b).to_string(),
    }
}

/// Project one selected occurrence. `None` clears this occurrence's display.
///
/// Callers select/order multiple returned overlays outside this adapter. A
/// missing event also requires the caller to clear its prior display. Invalid
/// monotonic time fails closed. Cancellation and epoch supersession clear
/// immediately; expected duration never manufactures end confirmation.
#[must_use]
pub fn project_event_overlay(
    event: &SourceEvent,
    clock: &PresentationClock,
    binding: &EventOverlayBinding,
    observation: OverlayObservationTime,
) -> Option<EventOverlay> {
    if event.key.source_id != binding.source_id
        || binding
            .event_id
            .as_ref()
            .is_some_and(|id| id != &event.key.event_id)
        || binding
            .origin
            .as_ref()
            .is_some_and(|origin| origin != &event.key.origin)
    {
        return None;
    }
    // Presence withdrawal is independent of timing freshness. Do not let a
    // stale clock resurrect an already cleared presence badge as uncertainty.
    // Independent boundary revisions cannot establish reappearance ordering;
    // reappearance therefore requires a new occurrence. Explicit non-vendor
    // timed boundaries retain their own lifecycle, never ended by absence.
    let presence_only = matches!(event.key.origin, EventOrigin::VendorMarker { .. })
        || !event.observations().any(|o| {
            matches!(
                o.boundary,
                EventBoundary::Start {
                    pts_ns: Some(_),
                    ..
                } | EventBoundary::End { pts_ns: Some(_) }
            )
        });
    if presence_only
        && event
            .observations()
            .any(|o| matches!(o.boundary, EventBoundary::MarkerAbsent))
    {
        return None;
    }
    let age = observation
        .now_ns
        .checked_sub(observation.lifecycle_since_ns)?;
    let projection = event.project(clock);
    let position = match clock {
        PresentationClock::Fresh {
            source_id,
            epoch,
            position_ns,
        } if source_id == &event.key.source_id && *epoch == event.key.epoch => Some(*position_ns),
        _ => None,
    };
    let start = event.observations().find_map(|o| match o.boundary {
        EventBoundary::Start { pts_ns, .. } => pts_ns,
        _ => None,
    });
    let (label, readout) = match projection.lifecycle {
        EventLifecycle::Cancelled | EventLifecycle::Superseded => return None,
        EventLifecycle::Announced => {
            let (start, position) = start.zip(position)?;
            if start.abs_diff(position) > binding.policy.warning_lead_ns {
                return None;
            }
            (
                OverlayLabel::Upcoming,
                Some(timer(OverlayTimerKind::UntilStart, start, position)),
            )
        }
        EventLifecycle::Active => {
            let position = position?;
            let readout = projection
                .confirmed_end_ns
                .or(projection.expected_end_ns)
                .map(|end| timer(OverlayTimerKind::Remaining, end, position))
                .or_else(|| start.map(|start| timer(OverlayTimerKind::Elapsed, position, start)));
            (OverlayLabel::Active, readout)
        }
        EventLifecycle::Ended => {
            if age >= binding.policy.terminal_hold_ns {
                return None;
            }
            (OverlayLabel::Ended, None)
        }
        EventLifecycle::Uncertain => {
            if age >= binding.policy.uncertain_hold_ns {
                return None;
            }
            // Only a fresh, same-epoch mapping and a genuinely elapsed expected
            // end support overrun. Contradictory explicit boundaries do not.
            match position.zip(projection.expected_end_ns) {
                Some((position, end))
                    if position >= end && projection.confirmed_end_ns.is_none() =>
                {
                    (
                        OverlayLabel::Overrun,
                        Some(timer(OverlayTimerKind::Overrun, position, end)),
                    )
                }
                _ => (OverlayLabel::Uncertain, None),
            }
        }
        EventLifecycle::Observed => (OverlayLabel::Observed, None),
    };
    Some(EventOverlay {
        key: OverlayEventKey::from(&event.key),
        lifecycle: projection.lifecycle,
        label,
        timer: readout,
        descriptor: event.descriptor().cloned(),
    })
}
