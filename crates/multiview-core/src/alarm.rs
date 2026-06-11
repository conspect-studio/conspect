//! The ITU-T X.733 alarm vocabulary as pure value types.
//!
//! This module is the **type foundation** the broadcast monitoring/alarm engine
//! (broadcast-multiviewer brief §4) builds on. It deliberately contains **no
//! state machine** — per-probe threshold/dwell/hysteresis and the actual
//! raising/clearing logic live in `multiview-engine`
//! (`multiview_engine::alarm::state::AlarmStateMachine`). Here we pin only the
//! shared, serialisable vocabulary every downstream crate agrees on:
//!
//! * [`PerceivedSeverity`] — the X.733 severity scale, with a **total order**
//!   (`Cleared < Indeterminate < Warning < Minor < Major < Critical`) so a
//!   probe -> tile -> group -> system **roll-up** is just the maximum
//!   ([`PerceivedSeverity::rollup`]).
//! * [`AlarmKind`] — the content-aware probe/fault taxonomy (black, freeze,
//!   silence, ...).
//! * [`AlarmId`] / [`AlarmScope`] — identity and the scope an alarm applies to.
//! * [`AckState`] — operator acknowledgement (who/when).
//! * [`AlarmRecord`] — a single alarm occurrence as a value type.
//!
//! Everything is pure, `serde`-serialisable, and documented; the severity
//! ordering and roll-up are the load-bearing invariants and are property-tested.
use serde::{Deserialize, Serialize};

use crate::time::MediaTime;

/// ITU-T X.733 perceived severity.
///
/// The variants are declared **in ascending order of urgency** and the derived
/// [`Ord`]/[`PartialOrd`] therefore yields the X.733 total order
/// `Cleared < Indeterminate < Warning < Minor < Major < Critical`. That makes a
/// probe -> tile -> group -> system roll-up the simple maximum over the set
/// (see [`PerceivedSeverity::rollup`]).
///
/// [`PerceivedSeverity::Cleared`] is the inactive / "no alarm" value and is the
/// [`Default`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash, Serialize, Deserialize,
)]
#[non_exhaustive]
pub enum PerceivedSeverity {
    /// The condition has cleared (or never fired): no active alarm.
    #[default]
    Cleared,
    /// Severity could not be determined.
    Indeterminate,
    /// A warning: a potential or impending problem.
    Warning,
    /// Minor: a non-service-affecting fault.
    Minor,
    /// Major: a service-affecting fault requiring urgent attention.
    Major,
    /// Critical: a service-affecting fault requiring immediate attention.
    Critical,
}

impl PerceivedSeverity {
    /// Whether this severity represents an **active** alarm (anything other than
    /// [`PerceivedSeverity::Cleared`]).
    #[must_use]
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Cleared)
    }

    /// Roll up a set of severities to the single worst (maximum) value.
    ///
    /// This is the X.733 probe -> tile -> group -> system aggregation: the
    /// rolled-up severity of a parent is the highest severity among its
    /// children. An empty iterator rolls up to [`PerceivedSeverity::Cleared`].
    #[must_use]
    pub fn rollup<I: IntoIterator<Item = Self>>(severities: I) -> Self {
        severities.into_iter().max().unwrap_or(Self::Cleared)
    }
}

/// The content-aware probe / fault taxonomy.
///
/// Each variant names a class of monitored condition (broadcast-multiviewer
/// brief §4). It is `#[non_exhaustive]` so additional probe kinds can be added
/// without a breaking change. Serialised **tagged** (the variant name), per repo
/// conventions — never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AlarmKind {
    /// The picture is black (below a luma threshold) for the dwell window.
    Black,
    /// The picture is frozen (successive frames are identical) for the dwell.
    Freeze,
    /// Audio silence (below a level threshold) for the dwell window.
    Silence,
    /// Audio level exceeds the permitted ceiling (over-level).
    OverLevel,
    /// Audio sample/true-peak clipping detected.
    Clip,
    /// A channel pair is phase-inverted (anti-correlated), risking mono collapse.
    PhaseInvert,
    /// Integrated/short-term loudness violates the target profile (EBU R128 /
    /// ATSC A/85).
    LoudnessViolation,
    /// Closed captions / subtitles expected but absent (presence loss).
    CaptionLoss,
    /// The signalled format/standard (resolution, frame rate, colorimetry, AFD)
    /// no longer matches what is expected.
    FormatMismatch,
    /// No usable signal on the input (loss of signal).
    SignalLoss,
}

/// A stable identifier for an alarm instance.
///
/// Opaque newtype over a `String`; construct with [`AlarmId::new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AlarmId(String);

impl AlarmId {
    /// Construct an alarm id from any string-like value.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What an alarm applies to, for roll-up and routing.
///
/// Serialised **tagged** (`#[serde(tag = "kind")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so finer scopes can be added later.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlarmScope {
    /// A single named probe (the leaf of the roll-up).
    Probe {
        /// Probe identifier.
        id: String,
    },
    /// A multiview tile, by zero-based index.
    Tile {
        /// Zero-based tile index.
        index: u32,
    },
    /// A named virtual/Boolean alarm group.
    Group {
        /// Group name.
        name: String,
    },
    /// The whole system (the root of the roll-up).
    System,
}

/// Operator acknowledgement state of an alarm.
///
/// Serialised **tagged** (`#[serde(tag = "state")]`); never `untagged`.
/// `#[non_exhaustive]` so future ack semantics (e.g. shelved) can be added.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state")]
#[non_exhaustive]
pub enum AckState {
    /// Not yet acknowledged by an operator.
    #[default]
    Unacked,
    /// Acknowledged: records who acknowledged it and when.
    Acked {
        /// Operator/user identifier that acknowledged the alarm.
        who: String,
        /// When the acknowledgement happened, on the media timeline.
        when: MediaTime,
    },
}

impl AckState {
    /// Build an acknowledged state from an operator id and acknowledgement time.
    #[must_use]
    pub fn acked(who: impl Into<String>, when: MediaTime) -> Self {
        Self::Acked {
            who: who.into(),
            when,
        }
    }

    /// Whether the alarm has been acknowledged.
    #[must_use]
    pub const fn is_acked(&self) -> bool {
        matches!(self, Self::Acked { .. })
    }
}

/// A single alarm occurrence as a pure value type.
///
/// This carries the X.733 attributes the engine and control plane share: the
/// probe [`kind`](AlarmRecord::kind), its [`severity`](AlarmRecord::severity),
/// the [`scope`](AlarmRecord::scope) it applies to, the media time it was
/// [`raised_at`](AlarmRecord::raised_at), how long the condition has persisted
/// ([`dwell`](AlarmRecord::dwell)), whether it is [`latched`](AlarmRecord::latched)
/// (held until explicitly reset), and its [`ack`](AlarmRecord::ack) state.
///
/// The transition/raising logic that produces and updates these records lives in
/// `multiview-engine` — this type is just the serialisable shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlarmRecord {
    /// Stable identity of this alarm instance.
    pub id: AlarmId,
    /// The probe/fault class that raised it.
    pub kind: AlarmKind,
    /// Current perceived severity (X.733).
    pub severity: PerceivedSeverity,
    /// What the alarm applies to.
    pub scope: AlarmScope,
    /// When the alarm was first raised, on the media timeline.
    pub raised_at: MediaTime,
    /// How long the underlying condition has persisted since `raised_at`.
    ///
    /// Zero at first raise; the engine advances it as the condition dwells.
    pub dwell: MediaTime,
    /// Whether the alarm is latched (held active until an explicit reset, even
    /// if the underlying condition clears).
    pub latched: bool,
    /// Operator acknowledgement state.
    pub ack: AckState,
}

impl AlarmRecord {
    /// Construct a fresh, unacknowledged, unlatched alarm record with zero dwell.
    #[must_use]
    pub fn new(
        id: AlarmId,
        kind: AlarmKind,
        severity: PerceivedSeverity,
        scope: AlarmScope,
        raised_at: MediaTime,
    ) -> Self {
        Self {
            id,
            kind,
            severity,
            scope,
            raised_at,
            dwell: MediaTime::ZERO,
            latched: false,
            ack: AckState::Unacked,
        }
    }

    /// Whether this record's severity represents an active alarm.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.severity.is_active()
    }
}
