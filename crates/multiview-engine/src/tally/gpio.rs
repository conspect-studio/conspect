//! A **virtual GPI/GPO model**: logical general-purpose input/output points with
//! level and edge semantics, as pure value machines (broadcast-multiviewer brief
//! §2, ADR-MV001).
//!
//! Physical GPI/GPO (relay closures, opto inputs) and their IP-native equivalent
//! AMWA NMOS IS-07 (event/tally over WebSocket/MQTT) are *transports*; this module
//! is the protocol-agnostic logic underneath. A [`GpiPoint`] consumes a raw level
//! sample and reports the level **and** the edge crossed (rising/falling) so an
//! automation scheduler can fire on a transition, not just a level. A [`GpoPoint`]
//! is the symmetric output: a logical state the engine drives outward.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Every point is a pure state machine — no I/O, no clock dependency beyond the
//! injected sample. Edge detection is a one-sample memory; the transport layer
//! pushes samples in and reads requests out. Nothing here blocks or `.await`s.
use serde::{Deserialize, Serialize};

/// Whether a point treats a closure/high as logically *active*, or inverts it.
///
/// GPI wiring is half normally-open, half normally-closed; the polarity lets the
/// logical layer present a consistent "active" regardless of the wire's resting
/// state. Serialised **tagged** by variant name (repo convention — never
/// `untagged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Polarity {
    /// A raw high/closed level is logically active (the default).
    #[default]
    ActiveHigh,
    /// A raw low/open level is logically active (the level is inverted).
    ActiveLow,
}

impl Polarity {
    /// Apply this polarity to a raw level, returning the logical *active* state.
    #[must_use]
    pub const fn apply(self, raw_high: bool) -> bool {
        match self {
            Self::ActiveHigh => raw_high,
            Self::ActiveLow => !raw_high,
        }
    }
}

/// The transition a [`GpiPoint`] crossed on a sample.
///
/// `None` is reported when the logical level is unchanged from the previous
/// sample (a steady level); `Rising`/`Falling` mark the instant of a crossing so
/// a scheduler can trigger exactly once per edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Edge {
    /// No transition this sample (level steady).
    #[default]
    None,
    /// The logical level went inactive → active.
    Rising,
    /// The logical level went active → inactive.
    Falling,
}

impl Edge {
    /// Whether this is a rising edge (inactive → active).
    #[must_use]
    pub const fn is_rising(self) -> bool {
        matches!(self, Self::Rising)
    }

    /// Whether this is a falling edge (active → inactive).
    #[must_use]
    pub const fn is_falling(self) -> bool {
        matches!(self, Self::Falling)
    }

    /// Whether any transition occurred this sample.
    #[must_use]
    pub const fn is_transition(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// A logical general-purpose **input** point with polarity + edge detection.
///
/// Drive it with [`GpiPoint::sample`] once per raw level reading; it returns the
/// [`Edge`] crossed and remembers the new level for next time. `Copy` so it can be
/// snapshotted without locking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpiPoint {
    polarity: Polarity,
    active: bool,
}

impl GpiPoint {
    /// Construct an input point with the given polarity, resting inactive.
    #[must_use]
    pub const fn new(polarity: Polarity) -> Self {
        Self {
            polarity,
            active: false,
        }
    }

    /// The current logical active level.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// The configured polarity.
    #[must_use]
    pub const fn polarity(&self) -> Polarity {
        self.polarity
    }

    /// Feed a raw level reading (`raw_high` = high/closed), returning the [`Edge`]
    /// crossed relative to the previous sample and updating the stored level.
    ///
    /// The raw level is first run through the [`Polarity`] to obtain the logical
    /// *active* state; the edge is then computed against the previously stored
    /// active state. A repeated identical level reports [`Edge::None`].
    pub fn sample(&mut self, raw_high: bool) -> Edge {
        let next = self.polarity.apply(raw_high);
        let edge = match (self.active, next) {
            (false, true) => Edge::Rising,
            (true, false) => Edge::Falling,
            _ => Edge::None,
        };
        self.active = next;
        edge
    }
}

/// A logical general-purpose **output** point the engine drives outward.
///
/// [`GpoPoint::request`] sets the desired logical state and returns the raw level
/// the transport should assert (honouring polarity); [`GpoPoint::raw_high`] reads
/// it back. `Copy` for lock-free snapshotting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpoPoint {
    polarity: Polarity,
    active: bool,
}

impl GpoPoint {
    /// Construct an output point with the given polarity, resting inactive.
    #[must_use]
    pub const fn new(polarity: Polarity) -> Self {
        Self {
            polarity,
            active: false,
        }
    }

    /// The current logical active state.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// The raw level the transport should assert for the current logical state,
    /// honouring [`Polarity`].
    #[must_use]
    pub const fn raw_high(&self) -> bool {
        match self.polarity {
            Polarity::ActiveHigh => self.active,
            Polarity::ActiveLow => !self.active,
        }
    }

    /// Set the desired logical `active` state, returning the raw level the
    /// transport should now assert.
    pub fn request(&mut self, active: bool) -> bool {
        self.active = active;
        self.raw_high()
    }
}
