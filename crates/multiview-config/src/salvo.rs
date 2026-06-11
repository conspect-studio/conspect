//! Salvo configuration (config-as-code): named atomic recalls.
//!
//! A [`Salvo`] is an operator-armed, atomically-taken bundle of state changes
//! (broadcast-multiviewer brief §8): recall a named **layout**, rebind a set of
//! **sources** into cells, force a set of **tally** states, and set **UMD**
//! label text — all applied as one make-before-break swap so the wall never
//! shows a half-applied state. This crate owns the declarative shape and its
//! reference validation; the arm/take execution lives in `multiview-engine`.
//!
//! All unions are **internally tagged** by `kind`, never `untagged` (ADR-0010).

use std::collections::HashSet;

use multiview_core::tally::TallyColor;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Rebind one cell to a managed source as part of a salvo recall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SourceRecall {
    /// The cell id whose source binding changes.
    pub cell: String,
    /// The managed source id to bind into the cell.
    pub input_id: String,
}

/// Force a cell's tally lamp to a fixed colour as part of a salvo recall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TallyRecall {
    /// The cell id whose tally is forced.
    pub cell: String,
    /// The lamp colour to assert.
    pub color: TallyColor,
}

/// Set a cell's Under-Monitor Display label text as part of a salvo recall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct UmdRecall {
    /// The cell id whose UMD label changes.
    pub cell: String,
    /// The label text to display.
    pub text: String,
}

/// A named, atomically-applied recall.
///
/// All four sub-recalls are optional, but a salvo must change **something**
/// (validation rejects an empty salvo). `layout` names a [`crate::schema::Layout`]
/// preset or a head layout by name; the source/tally/umd recalls reference cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Salvo {
    /// Stable salvo id (unique within the document).
    pub id: String,
    /// Human-friendly display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Recall a named layout (preset or head layout name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Source rebindings.
    #[serde(default)]
    pub sources: Vec<SourceRecall>,
    /// Forced tally states.
    #[serde(default)]
    pub tally: Vec<TallyRecall>,
    /// UMD label changes.
    #[serde(default)]
    pub umd: Vec<UmdRecall>,
}

impl Salvo {
    /// Validate this salvo's internal consistency (non-empty id, does something,
    /// no cell rebound to two sources, non-empty references).
    ///
    /// Cell- and source-reference resolution against the document is enforced by
    /// [`crate::MultiviewConfig::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an empty id, a salvo that changes
    /// nothing, a cell rebound to more than one source, or an empty cell/source
    /// reference.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a salvo has an empty id".to_owned(),
            ));
        }
        if self.layout.is_none()
            && self.sources.is_empty()
            && self.tally.is_empty()
            && self.umd.is_empty()
        {
            return Err(ConfigError::Validation(format!(
                "salvo {:?} changes nothing (no layout, sources, tally, or umd)",
                self.id
            )));
        }
        if let Some(layout) = &self.layout {
            if layout.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has an empty layout name",
                    self.id
                )));
            }
        }
        let mut seen_cells: HashSet<&str> = HashSet::with_capacity(self.sources.len());
        for recall in &self.sources {
            if recall.cell.is_empty() || recall.input_id.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has a source recall with an empty cell or input_id",
                    self.id
                )));
            }
            if !seen_cells.insert(recall.cell.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} rebinds cell {:?} more than once",
                    self.id, recall.cell
                )));
            }
        }
        for recall in &self.tally {
            if recall.cell.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has a tally recall with an empty cell",
                    self.id
                )));
            }
        }
        for recall in &self.umd {
            if recall.cell.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "salvo {:?} has a umd recall with an empty cell",
                    self.id
                )));
            }
        }
        Ok(())
    }
}
