//! Shared tally vocabulary (TSL UMD palette, brightness, bus sources).
//!
//! This module pins the broadcast tally **type foundation** (broadcast-
//! multiviewer brief §2). The TSL UMD protocol family carries per-element tally
//! as a 2-bit colour (`0 = off`, `1 = red`, `2 = green`, `3 = amber`) and a
//! 2-bit brightness. These pure value types are consumed later by
//! `multiview-overlay` (tally borders / UMD rendering), `multiview-engine` (the tally
//! arbiter), and `multiview-control` (TSL ingest/egress). No I/O, no protocol
//! framing — those belong to the TSL codecs in `multiview-input`/`multiview-output`.
use serde::{Deserialize, Serialize};

/// A tally lamp colour, matching the TSL UMD palette.
///
/// The discriminants are deliberately the TSL wire codes; use
/// [`TallyColor::tsl_code`] / [`TallyColor::from_tsl_code`] at the protocol
/// boundary. `#[non_exhaustive]` so future palettes (e.g. operator-custom
/// colours) can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TallyColor {
    /// Lamp off (TSL code 0).
    #[default]
    Off,
    /// Red — conventionally **program / on-air** (TSL code 1).
    Red,
    /// Green — conventionally **preview** (TSL code 2).
    Green,
    /// Amber — a conventional third/ISO state (TSL code 3).
    Amber,
}

impl TallyColor {
    /// The TSL UMD 2-bit colour code (`0..=3`) for this colour.
    #[must_use]
    pub const fn tsl_code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Red => 1,
            Self::Green => 2,
            Self::Amber => 3,
        }
    }

    /// Map a TSL UMD 2-bit colour code back to a [`TallyColor`].
    ///
    /// Returns [`None`] for any code outside `0..=3`.
    #[must_use]
    pub const fn from_tsl_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Off),
            1 => Some(Self::Red),
            2 => Some(Self::Green),
            3 => Some(Self::Amber),
            _ => None,
        }
    }

    /// Whether the lamp is lit (any colour other than [`TallyColor::Off`]).
    #[must_use]
    pub const fn is_lit(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// A TSL UMD 2-bit tally brightness level (`0..=3`).
///
/// Constructed via [`Brightness::new`], which saturates any value above the
/// 2-bit ceiling to `3`. [`Brightness::FULL`] is the [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Brightness(u8);

impl Brightness {
    /// Full brightness (level `3`).
    pub const FULL: Self = Self(3);
    /// The dimmest lit level (level `1`).
    pub const DIM: Self = Self(1);

    /// The maximum 2-bit level.
    const MAX_LEVEL: u8 = 3;

    /// Construct a brightness, saturating any value above the 2-bit ceiling to
    /// `3`.
    #[must_use]
    pub const fn new(level: u8) -> Self {
        if level > Self::MAX_LEVEL {
            Self(Self::MAX_LEVEL)
        } else {
            Self(level)
        }
    }

    /// The 2-bit brightness level (`0..=3`).
    #[must_use]
    pub const fn level(self) -> u8 {
        self.0
    }
}

impl Default for Brightness {
    fn default() -> Self {
        Self::FULL
    }
}

/// The mixer/router bus a tally state originates from.
///
/// Serialised **tagged** (`#[serde(tag = "kind")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so additional bus kinds can be added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BusSource {
    /// The program / on-air bus (conventionally red tally).
    Program,
    /// The preview / next bus (conventionally green tally).
    Preview,
    /// An auxiliary bus, by zero-based index.
    Aux {
        /// Zero-based aux bus index.
        index: u32,
    },
    /// An isolated (ISO) record bus, by zero-based index.
    Iso {
        /// Zero-based ISO bus index.
        index: u32,
    },
}

/// The resolved tally state of a single tile / monitored element.
///
/// Combines the lamp [`color`](TallyState::color), its
/// [`brightness`](TallyState::brightness), and the [`source`](TallyState::source)
/// bus that produced it. Produced by the tally arbiter (later, in
/// `multiview-engine`) and rendered by `multiview-overlay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyState {
    /// The lamp colour.
    pub color: TallyColor,
    /// The lamp brightness.
    pub brightness: Brightness,
    /// Which bus this state came from.
    pub source: BusSource,
}

impl TallyState {
    /// A program (red, full-brightness) tally.
    #[must_use]
    pub const fn program() -> Self {
        Self {
            color: TallyColor::Red,
            brightness: Brightness::FULL,
            source: BusSource::Program,
        }
    }

    /// A preview (green, full-brightness) tally.
    #[must_use]
    pub const fn preview() -> Self {
        Self {
            color: TallyColor::Green,
            brightness: Brightness::FULL,
            source: BusSource::Preview,
        }
    }

    /// Whether the tally lamp is lit.
    #[must_use]
    pub const fn is_lit(self) -> bool {
        self.color.is_lit()
    }
}

impl Default for TallyState {
    /// An unlit (off) program-bus tally at full brightness.
    fn default() -> Self {
        Self {
            color: TallyColor::Off,
            brightness: Brightness::FULL,
            source: BusSource::Program,
        }
    }
}
