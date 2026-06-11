//! The tally **profile**: the configurable bit↔colour and index↔tile mappings
//! that turn an external tally bus's wire vocabulary into Multiview's per-tile
//! [`TallyState`] (broadcast-multiviewer brief §2, ADR-MV001).
//!
//! An external switcher/router/GPI bus speaks in *its own* numbering: a router
//! crosspoint index, a GPI pin number, an IS-07 source label. The profile is the
//! pure, serialisable lookup table the [`arbiter`](crate::tally::arbiter) consults
//! to answer two questions for each incoming tally fact:
//!
//! * **bit → colour**: which [`TallyColor`] does a given bus *bit* light? (PGM is
//!   conventionally red, PVW green, an aux/ISO bus amber — but it is configurable.)
//! * **index → tile**: which multiview tile does a given bus *element index* drive?
//!
//! Everything here is a pure value type — no I/O, no clock, no channel — so the
//! whole mapping is exhaustively property-testable.
use std::collections::BTreeMap;

use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use serde::{Deserialize, Serialize};

/// A single bus *bit* and the [`TallyColor`] / [`BusSource`] it asserts.
///
/// A tally bus exposes a set of independent on/off bits (PGM, PVW, aux 1, …);
/// each [`BitMapping`] says "when *this* bit is set for an element, light it with
/// *this* colour, attributing it to *this* bus". The [`priority`](BitMapping::priority)
/// breaks ties when several bits are simultaneously set on one element (higher
/// wins) under the [`Priority`](super::arbiter::ConflictPolicy::Priority) policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitMapping {
    /// Zero-based bus bit number this rule matches.
    pub bit: u8,
    /// The lamp colour this bit asserts when set.
    pub color: TallyColor,
    /// The bus this bit is attributed to (for the resolved [`TallyState`]).
    pub source: BusSource,
    /// Tie-break priority when several bits are set at once (higher wins).
    pub priority: u8,
}

impl BitMapping {
    /// Construct a bit→colour mapping with the given tie-break priority.
    #[must_use]
    pub const fn new(bit: u8, color: TallyColor, source: BusSource, priority: u8) -> Self {
        Self {
            bit,
            color,
            source,
            priority,
        }
    }
}

/// A configurable tally profile: bit→colour rules plus an index→tile remap.
///
/// Construct an empty profile with [`TallyProfile::new`] and add rules with the
/// builder methods, or deserialise one from config. The arbiter consumes it
/// read-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyProfile {
    /// Bit→colour rules, keyed by bus bit for O(log n) lookup and stable
    /// (sorted) serialisation.
    bits: BTreeMap<u8, BitMapping>,
    /// Index→tile remap: external bus element index → multiview tile index. An
    /// index absent from the map is treated as identity (`index == tile`) unless
    /// [`strict_index`](TallyProfile::strict_index) is set.
    index_to_tile: BTreeMap<u32, u32>,
    /// When `true`, an external index absent from `index_to_tile` maps to **no**
    /// tile (returns [`None`]); when `false` (the default) it maps identically.
    strict_index: bool,
    /// Brightness applied to every resolved [`TallyState`] from this profile.
    brightness: Brightness,
}

impl TallyProfile {
    /// An empty profile: no bit rules, identity index mapping, full brightness.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bits: BTreeMap::new(),
            index_to_tile: BTreeMap::new(),
            strict_index: false,
            brightness: Brightness::FULL,
        }
    }

    /// Builder: add (or replace) a bit→colour mapping. The latest rule for a bit
    /// wins.
    #[must_use]
    pub fn with_bit(mut self, mapping: BitMapping) -> Self {
        self.bits.insert(mapping.bit, mapping);
        self
    }

    /// Builder: map an external element `index` to multiview `tile`.
    #[must_use]
    pub fn with_index(mut self, index: u32, tile: u32) -> Self {
        self.index_to_tile.insert(index, tile);
        self
    }

    /// Builder: require an explicit index mapping (unmapped indices resolve to
    /// [`None`] rather than identity).
    #[must_use]
    pub const fn strict_index(mut self) -> Self {
        self.strict_index = true;
        self
    }

    /// Builder: set the brightness stamped on every resolved [`TallyState`].
    #[must_use]
    pub const fn with_brightness(mut self, brightness: Brightness) -> Self {
        self.brightness = brightness;
        self
    }

    /// The brightness this profile stamps on resolved states.
    #[must_use]
    pub const fn brightness(&self) -> Brightness {
        self.brightness
    }

    /// Look up the [`BitMapping`] for bus bit `bit`, if any.
    #[must_use]
    pub fn bit(&self, bit: u8) -> Option<BitMapping> {
        self.bits.get(&bit).copied()
    }

    /// Map an external bus element `index` to a multiview tile index.
    ///
    /// Returns the explicit mapping when present; otherwise identity
    /// (`Some(index)`) unless [`strict_index`](TallyProfile::strict_index) was
    /// set, in which case an unmapped index returns [`None`].
    #[must_use]
    pub fn tile_for(&self, index: u32) -> Option<u32> {
        match self.index_to_tile.get(&index) {
            Some(&tile) => Some(tile),
            None if self.strict_index => None,
            None => Some(index),
        }
    }

    /// Resolve the highest-priority lit bit among `set_bits` into a
    /// [`TallyState`] (colour + this profile's brightness + originating bus).
    ///
    /// `set_bits` are the bus bits currently asserted for one element. Only bits
    /// that have a [`BitMapping`] **and** light a non-[`TallyColor::Off`] colour
    /// are considered; the winner is the one with the highest
    /// [`priority`](BitMapping::priority), ties broken by the lower bit number
    /// (deterministic). Returns [`None`] when no mapped bit is lit.
    #[must_use]
    pub fn resolve_bits<I: IntoIterator<Item = u8>>(&self, set_bits: I) -> Option<TallyState> {
        let mut best: Option<BitMapping> = None;
        for bit in set_bits {
            let Some(mapping) = self.bits.get(&bit).copied() else {
                continue;
            };
            if !mapping.color.is_lit() {
                continue;
            }
            best = Some(match best {
                None => mapping,
                Some(current) => pick_higher(current, mapping),
            });
        }
        best.map(|mapping| TallyState {
            color: mapping.color,
            brightness: self.brightness,
            source: mapping.source,
        })
    }
}

/// Pick the higher-priority of two mappings; ties go to the lower bit number so
/// resolution is deterministic regardless of iteration order.
fn pick_higher(current: BitMapping, candidate: BitMapping) -> BitMapping {
    if candidate.priority > current.priority
        || (candidate.priority == current.priority && candidate.bit < current.bit)
    {
        candidate
    } else {
        current
    }
}
