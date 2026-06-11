//! The ITU-T X.733 alarm **state machine**: dwell-up / dwell-down hysteresis,
//! latch and operator acknowledge, as a pure function of an injected
//! [`MediaTime`] (ADR-MV001 / broadcast-multiviewer §4).
//!
//! A probe reports an instantaneous *condition present / absent* sample each tick
//! (see [`crate::probe`]); this machine decides when that condition has persisted
//! long enough to **raise** an alarm and when its absence has persisted long
//! enough to **clear** one. Two independent dwells give the hysteresis that stops
//! a flapping source from flapping the alarm:
//!
//! ```text
//!  Clear ──present──▶ Pending ──(held ≥ dwell_up)──▶ Raised
//!    ▲                  │                              │
//!    │             absent│  ▲                     absent│
//!    │ (held ≥ dwell_down)  └────────present────────────┘
//!    │                  ▼                              ▼
//!    └──────────────  (Clear)        Raised ◀─present─ Clearing
//!                                       └──(held ≥ dwell_down)──▶ Clear
//! ```
//!
//! Mirrors the pure-state-machine pattern of [`multiview_framestore::state`] and
//! [`multiview_overlay::alert`]: the whole transition table is total over
//! `(phase, present, now)` and is exhaustively property-testable with a synthetic
//! injected clock — **no real time, no sleeps** (ADR-MV001 synthetic-fault
//! requirement).
use multiview_core::alarm::{
    AckState, AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity,
};
use multiview_core::time::MediaTime;

/// The dwell windows that give an alarm its hysteresis.
///
/// `dwell_up` is how long the fault condition must be **continuously present**
/// before the alarm raises; `dwell_down` is how long it must be **continuously
/// absent** before a raised alarm clears. Both are durations on the media
/// timeline; either may be [`MediaTime::ZERO`] for an instantaneous raise/clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmHysteresis {
    dwell_up: MediaTime,
    dwell_down: MediaTime,
}

impl Default for AlarmHysteresis {
    /// Zero dwell in both directions (raise/clear immediately) — the neutral
    /// element; production probes set real windows.
    fn default() -> Self {
        Self {
            dwell_up: MediaTime::ZERO,
            dwell_down: MediaTime::ZERO,
        }
    }
}

impl AlarmHysteresis {
    /// Construct from explicit dwell-up and dwell-down windows.
    ///
    /// Negative inputs are clamped to [`MediaTime::ZERO`] so a window is never
    /// "in the past".
    #[must_use]
    pub fn new(dwell_up: MediaTime, dwell_down: MediaTime) -> Self {
        Self {
            dwell_up: non_negative(dwell_up),
            dwell_down: non_negative(dwell_down),
        }
    }

    /// Construct hysteresis from a config [`Dwell`](multiview_config::probe::Dwell),
    /// converting its millisecond raise/clear windows to the media timeline.
    ///
    /// This is the conversion the engine uses to honour an operator-authored
    /// probe declaration; the windows are non-negative by construction (`u32`
    /// milliseconds), so no clamping is needed.
    #[must_use]
    pub fn from_dwell(dwell: multiview_config::probe::Dwell) -> Self {
        Self {
            dwell_up: millis_to_media_time(dwell.up_ms),
            dwell_down: millis_to_media_time(dwell.down_ms),
        }
    }

    /// The raise (dwell-up) window.
    #[must_use]
    pub const fn dwell_up(self) -> MediaTime {
        self.dwell_up
    }

    /// The clear (dwell-down) window.
    #[must_use]
    pub const fn dwell_down(self) -> MediaTime {
        self.dwell_down
    }
}

/// The lifecycle phase of an alarm condition.
///
/// `Pending`/`Clearing` are the *transitional* phases where a dwell is being
/// served; `Clear`/`Raised` are stable. `since` records when the current
/// transitional phase began so the dwell can be measured against `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
    /// No active alarm and the condition is absent.
    Clear,
    /// The condition is present but `dwell_up` has not yet elapsed; began at the
    /// carried instant.
    Pending {
        /// When the condition first became continuously present.
        since: MediaTime,
    },
    /// The alarm is active (the condition was present for `dwell_up`).
    Raised,
    /// The alarm is active but the condition has gone absent; `dwell_down` has
    /// not yet elapsed; began at the carried instant.
    Clearing {
        /// When the condition first became continuously absent.
        since: MediaTime,
    },
}

impl Phase {
    /// Whether an alarm is currently active (drawn / reported): `Raised` or
    /// `Clearing` (still held through the clear dwell).
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Raised | Self::Clearing { .. })
    }
}

/// What changed as a result of an [`AlarmStateMachine`] step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AlarmTransition {
    /// No active-state change this step (still clear, or still active).
    None,
    /// The alarm became active this step (`… → Raised`).
    Raised,
    /// The alarm became inactive this step (`… → Clear`).
    Cleared,
}

/// An X.733 alarm instance with dwell/latch/ack hysteresis.
///
/// Construct with [`AlarmStateMachine::new`], then drive it once per control
/// tick with [`AlarmStateMachine::observe`] (the probe's condition + the current
/// [`MediaTime`]). [`AlarmStateMachine::record`] snapshots the X.733
/// [`AlarmRecord`] for the realtime/event layer. The type is `Clone` so a
/// publisher can snapshot it without locking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmStateMachine {
    id: AlarmId,
    kind: AlarmKind,
    scope: AlarmScope,
    /// The severity this alarm carries while raised (X.733 perceived severity).
    active_severity: PerceivedSeverity,
    hysteresis: AlarmHysteresis,
    /// When `true`, a raised alarm stays active even after the condition clears,
    /// until [`AlarmStateMachine::reset`] is called (X.733 latch).
    latching: bool,
    phase: Phase,
    /// Set once the alarm latches (raised while `latching`); blocks auto-clear.
    latched: bool,
    ack: AckState,
    /// When the alarm most recently raised (for the record + dwell reporting).
    raised_at: MediaTime,
}

impl AlarmStateMachine {
    /// Construct a fresh, clear alarm for `(id, kind, scope)` that raises at
    /// `active_severity` with the given `hysteresis`. Non-latching by default;
    /// enable latch with [`AlarmStateMachine::latching`].
    #[must_use]
    pub fn new(
        id: AlarmId,
        kind: AlarmKind,
        scope: AlarmScope,
        active_severity: PerceivedSeverity,
        hysteresis: AlarmHysteresis,
    ) -> Self {
        Self {
            id,
            kind,
            scope,
            active_severity,
            hysteresis,
            latching: false,
            phase: Phase::Clear,
            latched: false,
            ack: AckState::Unacked,
            raised_at: MediaTime::ZERO,
        }
    }

    /// Enable X.733 latching: a raised alarm stays active after the condition
    /// clears until [`AlarmStateMachine::reset`].
    #[must_use]
    pub const fn latching(mut self) -> Self {
        self.latching = true;
        self
    }

    /// Build a fresh, clear alarm state machine from an operator-authored config
    /// [`Probe`](multiview_config::probe::Probe).
    ///
    /// This is the integration seam between the declarative probe layer
    /// ([`multiview_config::probe`]) and the engine X.733 lifecycle: the probe's
    /// `id` becomes the [`AlarmId`] and (as the watched leaf) the
    /// [`AlarmScope::Probe`]; its [`ProbeKind`](multiview_config::probe::ProbeKind)
    /// maps to the [`AlarmKind`] it raises; its millisecond
    /// [`Dwell`](multiview_config::probe::Dwell) becomes the [`AlarmHysteresis`];
    /// and its `severity`/`latched` policy is carried verbatim. The probe is read
    /// only — the engine never mutates the config.
    #[must_use]
    pub fn from_probe(probe: &multiview_config::probe::Probe) -> Self {
        let machine = Self::new(
            AlarmId::new(probe.id.clone()),
            probe.kind.alarm_kind(),
            AlarmScope::Probe {
                id: probe.id.clone(),
            },
            probe.severity,
            AlarmHysteresis::from_dwell(probe.dwell),
        );
        if probe.latched {
            machine.latching()
        } else {
            machine
        }
    }

    /// The alarm's stable identity.
    #[must_use]
    pub const fn id(&self) -> &AlarmId {
        &self.id
    }

    /// The fault class this alarm reports.
    #[must_use]
    pub const fn kind(&self) -> AlarmKind {
        self.kind
    }

    /// The scope this alarm applies to.
    #[must_use]
    pub const fn scope(&self) -> &AlarmScope {
        &self.scope
    }

    /// The current lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the alarm is currently active (`Raised` or `Clearing`).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.phase.is_active()
    }

    /// Whether the alarm is latched (held active until [`reset`](Self::reset)).
    #[must_use]
    pub const fn is_latched(&self) -> bool {
        self.latched
    }

    /// The severity to report **now**: the active severity while the alarm is
    /// active, otherwise [`PerceivedSeverity::Cleared`].
    #[must_use]
    pub const fn current_severity(&self) -> PerceivedSeverity {
        if self.phase.is_active() {
            self.active_severity
        } else {
            PerceivedSeverity::Cleared
        }
    }

    /// The acknowledgement state.
    #[must_use]
    pub const fn ack(&self) -> &AckState {
        &self.ack
    }

    /// Acknowledge the alarm (operator `who` at media time `when`). A no-op
    /// unless the alarm is currently active — you cannot acknowledge a clear
    /// alarm.
    pub fn acknowledge(&mut self, who: impl Into<String>, when: MediaTime) {
        if self.phase.is_active() {
            self.ack = AckState::acked(who, when);
        }
    }

    /// Explicitly reset a latched (or any active) alarm back to clear, and clear
    /// the acknowledgement. Used for the X.733 latch reset; after a reset the
    /// alarm re-evaluates from the next [`observe`](Self::observe).
    pub fn reset(&mut self) {
        self.phase = Phase::Clear;
        self.latched = false;
        self.ack = AckState::Unacked;
    }

    /// Drive the machine one step with the probe's `condition_present` reading at
    /// media time `now`, applying dwell-up/dwell-down hysteresis and latch.
    ///
    /// This is the classify-style transition — a total function of
    /// `(phase, condition_present, now)` — and returns whether the active state
    /// changed this step:
    ///
    /// * `Clear` + present → `Pending{since: now}` (start the raise dwell);
    /// * `Pending` + present, held `>= dwell_up` → `Raised` ([`AlarmTransition::Raised`]);
    /// * `Pending` + absent → back to `Clear` (the candidate fault went away);
    /// * `Raised` + absent → `Clearing{since: now}` (start the clear dwell),
    ///   **unless latched**, in which case it stays `Raised`;
    /// * `Clearing` + absent, held `>= dwell_down` → `Clear` ([`AlarmTransition::Cleared`]);
    /// * `Clearing` + present → back to `Raised` (the fault returned within the
    ///   clear dwell — anti-flap);
    /// * a latched alarm never auto-clears: an absent condition holds `Raised`.
    ///
    /// `now` running backwards (non-monotonic) cannot shorten a dwell: elapsed
    /// time is clamped to zero, so a backwards step never prematurely raises or
    /// clears.
    pub fn observe(&mut self, condition_present: bool, now: MediaTime) -> AlarmTransition {
        // First fold the new sample into the phase (a *transitional* phase carries
        // the instant the current run of present/absent samples began), then
        // evaluate the dwell against `now`. Evaluating after the fold means a
        // **zero dwell** raises/clears on the very first qualifying sample, while
        // a positive dwell still requires the run to persist.
        match self.phase {
            Phase::Clear => {
                if condition_present {
                    self.phase = Phase::Pending { since: now };
                }
            }
            Phase::Pending { .. } => {
                if !condition_present {
                    self.phase = Phase::Clear;
                }
            }
            Phase::Raised => {
                if !condition_present && !self.latched {
                    self.phase = Phase::Clearing { since: now };
                }
            }
            Phase::Clearing { .. } => {
                if condition_present || self.latched {
                    // Fault returned within the clear dwell (or a latched alarm
                    // should never be clearing): snap back to active.
                    self.phase = Phase::Raised;
                }
            }
        }

        // Now serve any due dwell on the resulting transitional phase.
        match self.phase {
            Phase::Pending { since } if elapsed(since, now) >= self.hysteresis.dwell_up => {
                self.do_raise(now);
                AlarmTransition::Raised
            }
            Phase::Clearing { since }
                if !self.latched && elapsed(since, now) >= self.hysteresis.dwell_down =>
            {
                self.phase = Phase::Clear;
                self.ack = AckState::Unacked;
                AlarmTransition::Cleared
            }
            _ => AlarmTransition::None,
        }
    }

    /// Snapshot the current X.733 [`AlarmRecord`] at media time `now`.
    ///
    /// The record's `severity` is [`current_severity`](Self::current_severity),
    /// its `dwell` is how long the alarm has been continuously active since
    /// `raised_at` (zero when clear), and `latched`/`ack` reflect the machine.
    #[must_use]
    pub fn record(&self, now: MediaTime) -> AlarmRecord {
        let dwell = if self.phase.is_active() {
            non_negative(now.saturating_sub(self.raised_at))
        } else {
            MediaTime::ZERO
        };
        AlarmRecord {
            id: self.id.clone(),
            kind: self.kind,
            severity: self.current_severity(),
            scope: self.scope.clone(),
            raised_at: self.raised_at,
            dwell,
            latched: self.latched,
            ack: self.ack.clone(),
        }
    }

    /// Transition into the raised state, recording the raise time and latch.
    fn do_raise(&mut self, now: MediaTime) {
        self.phase = Phase::Raised;
        self.raised_at = now;
        if self.latching {
            self.latched = true;
        }
    }
}

/// Elapsed time from `since` to `now`, clamped to non-negative so a clock that
/// momentarily runs backwards cannot shorten a dwell.
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

/// Convert a non-negative millisecond window to a [`MediaTime`].
///
/// `u32` milliseconds times 1e6 ns fits in `i64` (`u32::MAX * 1_000_000`
/// ≈ 4.29e15 < `i64::MAX`), so the widen-then-multiply is exact with no overflow.
fn millis_to_media_time(ms: u32) -> MediaTime {
    MediaTime::from_nanos(i64::from(ms).saturating_mul(1_000_000))
}
