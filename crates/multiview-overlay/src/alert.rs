//! Alert-card state model.
//!
//! The alert card is a *must-never-fail* overlay element (the SIGNAL LOST class
//! of ADR-R008): its assets are atlas-resident so it can be drawn at the exact
//! instant of failure. Crucially, its visibility is driven purely by this small,
//! deterministic state machine over the engine's media clock — **never** by a
//! live decoded input frame (overlays are input-decoupled, ADR-R008), so the
//! alert path remains drawable even when every input and the GPU are gone.
//!
//! ```text
//!   Idle ──raise──▶ Active ──acknowledge──▶ Acknowledged
//!     ▲                │  ▲                      │
//!     │            clear│  └────────raise────────┘
//!     │                ▼
//!     └──tick(dwell)── Clearing ──raise──▶ Active
//! ```
//!
//! `clear` does not hide the card immediately; it enters a `Clearing` *dwell* so
//! a flapping condition (e.g. a tile that reconnects then drops again) does not
//! flicker the card on and off. The card hides only once [`AlertCard::tick`] is
//! called at or after the dwell deadline.

use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

/// How serious an alert is. Ordered `Info < Warning < Critical` so a UI/renderer
/// can pick the worst active alert or sort by urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Severity {
    /// Informational; lowest urgency.
    #[default]
    Info,
    /// A degraded but recoverable condition (reconnecting, encoder recycle).
    Warning,
    /// A hard failure the operator must see (SIGNAL LOST, GPU device loss).
    Critical,
}

/// The lifecycle state of an [`AlertCard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AlertState {
    /// Not firing; the card is hidden.
    #[default]
    Idle,
    /// Firing and demanding attention; visible.
    Active,
    /// Firing, still visible, but the operator has acknowledged it.
    Acknowledged,
    /// The condition cleared; the card is held visible through its dwell window
    /// to suppress flicker, then transitions to [`AlertState::Idle`].
    Clearing,
}

impl AlertState {
    /// Whether a card in this state is drawn on the canvas.
    #[must_use]
    pub const fn is_visible(self) -> bool {
        matches!(self, Self::Active | Self::Acknowledged | Self::Clearing)
    }
}

/// An alert card: a label, a severity, a clear-dwell window, and its current
/// [`AlertState`].
///
/// Driven by three inputs only — [`AlertCard::raise`], [`AlertCard::clear`],
/// [`AlertCard::acknowledge`] — plus [`AlertCard::tick`] to advance the dwell.
/// Construct with [`AlertCard::new`]; this type is `Clone` so a renderer can
/// snapshot it without locking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertCard {
    /// Operator-facing label (e.g. `"tile 3: SIGNAL LOST"`).
    pub label: String,
    severity: Severity,
    state: AlertState,
    /// How long to keep the card visible after the condition clears.
    clear_dwell: MediaTime,
    /// When the current `Clearing` dwell ends. Only meaningful in
    /// [`AlertState::Clearing`].
    clearing_deadline: MediaTime,
}

impl AlertCard {
    /// Create an idle (hidden) card with the given label and severity and a zero
    /// clear-dwell. Add a dwell with [`AlertCard::with_clear_dwell`].
    #[must_use]
    pub fn new(label: impl Into<String>, severity: Severity) -> Self {
        Self {
            label: label.into(),
            severity,
            state: AlertState::Idle,
            clear_dwell: MediaTime::ZERO,
            clearing_deadline: MediaTime::ZERO,
        }
    }

    /// Set the clear-dwell window (anti-flicker hold after [`AlertCard::clear`]).
    #[must_use]
    pub fn with_clear_dwell(mut self, dwell: MediaTime) -> Self {
        self.clear_dwell = dwell;
        self
    }

    /// The card's current state.
    #[must_use]
    pub const fn state(&self) -> AlertState {
        self.state
    }

    /// The card's severity.
    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.severity
    }

    /// The configured clear-dwell window.
    #[must_use]
    pub const fn clear_dwell(&self) -> MediaTime {
        self.clear_dwell
    }

    /// Whether the card is currently drawn.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.state.is_visible()
    }

    /// Raise (fire) the alert at media time `now`.
    ///
    /// From any state this moves to [`AlertState::Active`], so a fresh
    /// occurrence re-demands attention even if the previous one was acknowledged
    /// or already clearing. `now` is unused today but is part of the contract so
    /// future rate-limiting/first-seen tracking needs no signature change.
    pub fn raise(&mut self, now: MediaTime) {
        let _ = now;
        self.state = AlertState::Active;
    }

    /// Acknowledge the alert (operator has seen it). Keeps the card visible.
    ///
    /// A no-op unless the card is [`AlertState::Active`] — you cannot
    /// acknowledge an idle or already-clearing card.
    pub fn acknowledge(&mut self) {
        if self.state == AlertState::Active {
            self.state = AlertState::Acknowledged;
        }
    }

    /// Signal that the underlying condition has cleared, at media time `now`.
    ///
    /// Enters the [`AlertState::Clearing`] dwell (still visible) until `now +
    /// clear_dwell`; the card hides only when [`AlertCard::tick`] reaches that
    /// deadline. A no-op when the card is already idle.
    pub fn clear(&mut self, now: MediaTime) {
        if self.state == AlertState::Idle {
            return;
        }
        self.clearing_deadline = now.saturating_add(self.clear_dwell);
        self.state = AlertState::Clearing;
    }

    /// Advance the dwell using the current media time `now`.
    ///
    /// Only affects a [`AlertState::Clearing`] card: once `now` reaches the
    /// clearing deadline the card transitions to [`AlertState::Idle`] (hidden).
    /// Inert in every other state.
    pub fn tick(&mut self, now: MediaTime) {
        if self.state == AlertState::Clearing && now >= self.clearing_deadline {
            self.state = AlertState::Idle;
        }
    }
}
