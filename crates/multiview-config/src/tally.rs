//! Tally-profile configuration (config-as-code).
//!
//! A [`TallyProfile`] is the declarative binding between an external tally bus
//! (a TSL UMD packet, a GPI bit, an NMOS IS-07 event) and the multiview: which
//! **bit** maps to which lamp **colour**, and which source **index** in the
//! protocol maps to which **cell** on the canvas (broadcast-multiviewer brief
//! §2). The live arbiter that consumes resolved tally lives in `multiview-engine`;
//! this crate owns only the schema and its validation.
//!
//! Colours are [`multiview_core::tally::TallyColor`] (the shared TSL palette); the
//! profile never invents its own colour vocabulary.

use std::collections::HashSet;

use multiview_core::tally::TallyColor;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// A single bit→colour rule: when `bit` is set in the incoming tally word, light
/// the lamp with `color`.
///
/// Lower bits are conventionally listed first, but order is not significant; the
/// profile validates that no bit is mapped twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct BitColor {
    /// The zero-based bit position in the incoming tally word.
    pub bit: u8,
    /// The lamp colour asserted when the bit is set.
    pub color: TallyColor,
}

/// A single protocol-index→cell rule: tally addressed to source `index` drives
/// the cell named `cell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IndexCell {
    /// The zero-based source/display index in the tally protocol.
    pub index: u32,
    /// The cell id the index resolves to.
    pub cell: String,
}

/// A named tally profile: the bit↔colour palette and the index↔cell address map
/// for one external tally bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TallyProfile {
    /// Stable profile id (unique within the document).
    pub id: String,
    /// Bit→colour rules.
    #[serde(default)]
    pub bit_colors: Vec<BitColor>,
    /// Protocol-index→cell address map.
    #[serde(default)]
    pub index_cells: Vec<IndexCell>,
}

impl TallyProfile {
    /// Validate this profile's internal consistency (no duplicate bit, no
    /// duplicate index, non-empty cell references).
    ///
    /// Cell-reference resolution against the document's cell set is enforced by
    /// [`crate::MultiviewConfig::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an empty id, a bit mapped to two
    /// colours, an index mapped to two cells, or an empty cell reference.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a tally profile has an empty id".to_owned(),
            ));
        }
        let mut seen_bits: HashSet<u8> = HashSet::with_capacity(self.bit_colors.len());
        for rule in &self.bit_colors {
            if !seen_bits.insert(rule.bit) {
                return Err(ConfigError::Validation(format!(
                    "tally profile {:?} maps bit {} more than once",
                    self.id, rule.bit
                )));
            }
        }
        let mut seen_indices: HashSet<u32> = HashSet::with_capacity(self.index_cells.len());
        for rule in &self.index_cells {
            if rule.cell.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "tally profile {:?} maps index {} to an empty cell",
                    self.id, rule.index
                )));
            }
            if !seen_indices.insert(rule.index) {
                return Err(ConfigError::Validation(format!(
                    "tally profile {:?} maps index {} more than once",
                    self.id, rule.index
                )));
            }
        }
        Ok(())
    }

    /// Resolve a protocol index to its bound cell id, if any.
    #[must_use]
    pub fn cell_for_index(&self, index: u32) -> Option<&str> {
        self.index_cells
            .iter()
            .find(|r| r.index == index)
            .map(|r| r.cell.as_str())
    }

    /// Resolve the lamp colour a tally word would assert, taking the
    /// highest-priority **lit** bit (the rule whose colour is not `Off`, latest
    /// in declaration order wins so finer rules can override coarse ones).
    #[must_use]
    pub fn color_for_word(&self, word: u32) -> TallyColor {
        let mut resolved = TallyColor::Off;
        for rule in &self.bit_colors {
            if word & (1u32 << u32::from(rule.bit)) != 0 && rule.color.is_lit() {
                resolved = rule.color;
            }
        }
        resolved
    }
}
