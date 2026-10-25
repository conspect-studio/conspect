//! The switcher configuration root (ADR-M012 §1).
//!
//! The switcher **domain** — mix/effects stages, downstream keyers, aux buses,
//! transition presets, wipe patterns and compositions — is landed by SW-17. This
//! module carries the root's **presence**, because that presence is already
//! load-bearing: [ADR-0059](../../docs/decisions/ADR-0059.md) §8 makes **program
//! audio** default-on when a `[switcher]` block is declared (a switcher without
//! program audio is not a switcher), so the crate must be able to tell a document
//! that declares the block from one that does not.
//!
//! The type has **no typed fields yet**: every key of the block is captured
//! verbatim by the flattened [`extra`](SwitcherConfig::extra) map, so a
//! `[switcher]` block authored against the ADR-M012 schema
//! (`[[switcher.mix_effects]]`, `[[switcher.transitions]]`, …) parses and
//! round-trips **losslessly** — through TOML and JSON alike — before SW-17 models
//! those sub-tables, instead of being silently emptied or refused over a field set
//! that has not landed. SW-17 replaces the map with the typed fields; the
//! presence ⇒ program-audio-default-on semantics do not change.

use serde::{Deserialize, Serialize};

/// The presence-root of the `[switcher]` config block (ADR-M012 §1).
///
/// Absent ⇒ no switcher is declared. Present ⇒ the document declares a switcher,
/// which is what makes program audio default-on (ADR-0059 §8); the full switcher
/// domain schema lands with SW-17.
///
/// Until then every unmodelled switcher field is carried verbatim in
/// [`extra`](Self::extra) so the block survives a TOML↔JSON round-trip and
/// [`ConfigDiff`](crate::ConfigDiff) can see edits inside it (restart-class).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SwitcherConfig {
    /// The unmodelled switcher fields, captured verbatim for lossless
    /// round-trip (the same passthrough shape as `Overlay::params`). A typed
    /// field that SW-17 adds takes over its key; until then nothing is dropped.
    ///
    /// Sorted (`serde_json::Map` is a `BTreeMap` without `preserve_order`) so
    /// serialization is deterministic and byte-stable.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
