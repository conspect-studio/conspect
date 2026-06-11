//! Map a **sustained** alarm to a layout action — "penalty box" a faulty tile
//! out of the wall, or auto-promote a spare into view — emitted as a
//! non-blocking engine command (ADR-MV001: "a penalty-box auto-promote-on-fault
//! layout action").
//!
//! This is a small pure state machine over an injected [`MediaTime`], one more
//! dwell layer on top of the alarm lifecycle: it only acts once an alarm has been
//! *continuously active* for a configured **sustain** window (so a brief alarm
//! never reshuffles the wall), and only reverts once the alarm has been
//! *continuously inactive* for a **release** window (anti-flap on the layout
//! itself).
//!
//! ## Isolation (invariant #1 + #10)
//!
//! [`PenaltyBox::observe`] **returns** a [`PenaltyAction`] — it never reaches
//! into the engine, never sends on a channel and never blocks. The engine polls
//! it on its slow control tick and applies any returned action at a frame
//! boundary (a Class-1 hot layout swap). A penalty-box decision can therefore
//! never stall the output clock or back-pressure anything; at worst the layout
//! change is applied one control tick later.
use multiview_core::time::MediaTime;

/// The layout action a sustained alarm requests.
///
/// These are *requests* the engine applies at a frame boundary, not mutations
/// performed here. The `tile` index identifies the affected cell in the active
/// layout (the same zero-based index as [`multiview_core::alarm::AlarmScope::Tile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PenaltyAction {
    /// No layout change this step.
    None,
    /// Remove the faulty `tile` from view (penalty box it) and, when a spare is
    /// configured, promote `promote` into its place.
    PenaltyBox {
        /// Zero-based index of the faulty tile to remove from view.
        tile: u32,
        /// Optional spare tile to auto-promote into the freed slot.
        promote: Option<u32>,
    },
    /// Restore a previously penalty-boxed `tile` (the alarm has recovered).
    Restore {
        /// Zero-based index of the tile to bring back into view.
        tile: u32,
    },
}

/// Configuration for a [`PenaltyBox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenaltyConfig {
    /// The faulty tile this penalty box guards.
    pub tile: u32,
    /// Optional spare tile to auto-promote when the faulty tile is boxed.
    pub promote: Option<u32>,
    /// How long the alarm must be **continuously active** before the tile is
    /// boxed (sustain window).
    pub sustain: MediaTime,
    /// How long the alarm must be **continuously inactive** before a boxed tile
    /// is restored (release window).
    pub release: MediaTime,
}

impl PenaltyConfig {
    /// Construct a config for `tile` with the given sustain/release windows and
    /// no auto-promote spare.
    #[must_use]
    pub fn new(tile: u32, sustain: MediaTime, release: MediaTime) -> Self {
        Self {
            tile,
            promote: None,
            sustain: non_negative(sustain),
            release: non_negative(release),
        }
    }

    /// Set a spare tile to auto-promote into the freed slot.
    #[must_use]
    pub const fn promote(mut self, spare: u32) -> Self {
        self.promote = Some(spare);
        self
    }
}

/// The penalty box's lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PenaltyState {
    /// The tile is in view and the alarm is inactive.
    Normal,
    /// The alarm is active but the sustain window has not yet elapsed; began at
    /// the carried instant.
    Arming {
        /// When the alarm most recently became continuously active.
        since: MediaTime,
    },
    /// The tile has been penalty-boxed (removed from view).
    Boxed,
    /// The tile is boxed but the alarm has gone inactive; the release window has
    /// not yet elapsed; began at the carried instant.
    Releasing {
        /// When the alarm most recently became continuously inactive.
        since: MediaTime,
    },
}

/// A per-tile penalty-box state machine.
///
/// Drive it once per control tick with [`PenaltyBox::observe`] — the guarded
/// alarm's *active* state plus the current [`MediaTime`] — and apply any returned
/// [`PenaltyAction`]. `Clone` so it can be snapshotted without locking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenaltyBox {
    config: PenaltyConfig,
    state: PenaltyState,
}

impl PenaltyBox {
    /// Construct a penalty box in the [`PenaltyState::Normal`] state.
    #[must_use]
    pub const fn new(config: PenaltyConfig) -> Self {
        Self {
            config,
            state: PenaltyState::Normal,
        }
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &PenaltyConfig {
        &self.config
    }

    /// The current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> PenaltyState {
        self.state
    }

    /// Whether the tile is currently removed from view (`Boxed` or `Releasing`).
    #[must_use]
    pub const fn is_boxed(&self) -> bool {
        matches!(
            self.state,
            PenaltyState::Boxed | PenaltyState::Releasing { .. }
        )
    }

    /// Drive the machine one step with the guarded alarm's `alarm_active` state at
    /// media time `now`, returning the layout action to apply (if any).
    ///
    /// Total over `(state, alarm_active, now)`:
    ///
    /// * `Normal` + active → `Arming{since: now}` (start the sustain dwell);
    /// * `Arming` + active, held `>= sustain` → `Boxed`, returns
    ///   [`PenaltyAction::PenaltyBox`];
    /// * `Arming` + inactive → back to `Normal` (the alarm cleared before
    ///   sustain);
    /// * `Boxed` + inactive → `Releasing{since: now}` (start the release dwell);
    /// * `Releasing` + inactive, held `>= release` → `Normal`, returns
    ///   [`PenaltyAction::Restore`];
    /// * `Releasing` + active → back to `Boxed` (the fault returned — anti-flap).
    ///
    /// A non-monotonic `now` cannot shorten a window (elapsed is clamped to
    /// zero).
    pub fn observe(&mut self, alarm_active: bool, now: MediaTime) -> PenaltyAction {
        // Fold the sample into the phase first (a transitional phase records when
        // the current active/inactive run began), then serve any due dwell. This
        // makes a **zero** sustain/release act on the first qualifying sample
        // while a positive window still requires persistence.
        match self.state {
            PenaltyState::Normal => {
                if alarm_active {
                    self.state = PenaltyState::Arming { since: now };
                }
            }
            PenaltyState::Arming { .. } => {
                if !alarm_active {
                    self.state = PenaltyState::Normal;
                }
            }
            PenaltyState::Boxed => {
                if !alarm_active {
                    self.state = PenaltyState::Releasing { since: now };
                }
            }
            PenaltyState::Releasing { .. } => {
                if alarm_active {
                    self.state = PenaltyState::Boxed;
                }
            }
        }

        match self.state {
            PenaltyState::Arming { since } if elapsed(since, now) >= self.config.sustain => {
                self.state = PenaltyState::Boxed;
                PenaltyAction::PenaltyBox {
                    tile: self.config.tile,
                    promote: self.config.promote,
                }
            }
            PenaltyState::Releasing { since } if elapsed(since, now) >= self.config.release => {
                self.state = PenaltyState::Normal;
                PenaltyAction::Restore {
                    tile: self.config.tile,
                }
            }
            _ => PenaltyAction::None,
        }
    }
}

/// Elapsed time from `since` to `now`, clamped non-negative.
fn elapsed(since: MediaTime, now: MediaTime) -> MediaTime {
    non_negative(now.saturating_sub(since))
}

/// Clamp a [`MediaTime`] to non-negative.
fn non_negative(t: MediaTime) -> MediaTime {
    if t.as_nanos() < 0 {
        MediaTime::ZERO
    } else {
        t
    }
}
