//! Caption-presence probe: a pure timeout state machine over the subtitle path.
//!
//! The probe watches caption/subtitle activity and raises a **caption-loss**
//! alarm ([`multiview_core::alarm::AlarmKind::CaptionLoss`]) when no caption is seen
//! within a configured timeout, clearing again when activity resumes. Like the
//! [`crate::alert`] card it is driven **only** by an injected media clock and by
//! explicit `observe_caption` calls — never by a live decode — so it cannot
//! back-pressure or stall the engine (invariant #10) and is exactly testable.
//!
//! Presence is reported as text ([`CaptionPresence::label`]) so the on-tile
//! badge conveys state without relying on colour (accessibility). The severity
//! is reported on the shared [`multiview_core::alarm::PerceivedSeverity`] scale so
//! the engine's roll-up can fold it in.

use multiview_core::alarm::{AlarmKind, PerceivedSeverity};
use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

/// Whether captions are currently considered present on the monitored path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptionPresence {
    /// Caption activity seen within the timeout window.
    #[default]
    Present,
    /// No caption activity within the timeout window.
    Lost,
}

impl CaptionPresence {
    /// A short text label (accessibility: presence conveyed by text, not colour).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Present => "captions present",
            Self::Lost => "captions lost",
        }
    }

    /// Whether captions are present.
    #[must_use]
    pub const fn is_present(self) -> bool {
        matches!(self, Self::Present)
    }
}

/// A caption-presence timeout probe.
///
/// Construct with [`CaptionProbe::new`] giving the inactivity `timeout`. Feed it
/// caption activity with [`CaptionProbe::observe_caption`] and advance it with
/// [`CaptionProbe::tick`]; it transitions to [`CaptionPresence::Lost`] once the
/// media clock reaches the deadline without intervening activity, and back to
/// [`CaptionPresence::Present`] on the next observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptionProbe {
    /// Inactivity window: captions are "lost" after this long with no activity.
    timeout: MediaTime,
    /// Media time by which a caption must be seen to remain "present". Starts at
    /// `timeout` (deadline measured from construction time `t0 = 0`).
    deadline: MediaTime,
    /// Current presence state.
    presence: CaptionPresence,
}

impl CaptionProbe {
    /// Create a probe with the given inactivity `timeout`.
    ///
    /// The probe starts [`CaptionPresence::Present`] with the first deadline at
    /// `timeout` (i.e. measured from construction time `t0 = 0`), so a path that
    /// never produces a caption is flagged lost once the timeout elapses.
    #[must_use]
    pub const fn new(timeout: MediaTime) -> Self {
        Self {
            timeout,
            deadline: timeout,
            presence: CaptionPresence::Present,
        }
    }

    /// The configured inactivity timeout.
    #[must_use]
    pub const fn timeout(&self) -> MediaTime {
        self.timeout
    }

    /// The current caption presence.
    #[must_use]
    pub const fn presence(&self) -> CaptionPresence {
        self.presence
    }

    /// The alarm kind this probe reports ([`AlarmKind::CaptionLoss`]).
    #[must_use]
    pub const fn alarm_kind(&self) -> AlarmKind {
        AlarmKind::CaptionLoss
    }

    /// The current severity on the X.733 scale: [`PerceivedSeverity::Cleared`]
    /// while captions are present, [`PerceivedSeverity::Minor`] once lost
    /// (caption loss is a non-service-affecting fault).
    #[must_use]
    pub const fn severity(&self) -> PerceivedSeverity {
        match self.presence {
            CaptionPresence::Present => PerceivedSeverity::Cleared,
            CaptionPresence::Lost => PerceivedSeverity::Minor,
        }
    }

    /// Record caption activity seen at media time `now`.
    ///
    /// Resets the inactivity deadline to `now + timeout` and restores
    /// [`CaptionPresence::Present`] (recovering from a prior loss).
    pub fn observe_caption(&mut self, now: MediaTime) {
        self.deadline = now.saturating_add(self.timeout);
        self.presence = CaptionPresence::Present;
    }

    /// Advance the probe to media time `now`.
    ///
    /// If `now` has reached the inactivity deadline the probe transitions to
    /// [`CaptionPresence::Lost`]. Idempotent once lost and safe under late or
    /// repeated ticks (it never flaps back to present without an observation).
    pub fn tick(&mut self, now: MediaTime) {
        if now >= self.deadline {
            self.presence = CaptionPresence::Lost;
        }
    }
}
