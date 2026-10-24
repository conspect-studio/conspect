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
//! The type therefore carries no fields yet, and deliberately does **not**
//! `deny_unknown_fields`: a `[switcher]` block authored against the ADR-M012
//! schema (`[[switcher.mix_effects]]`, `[[switcher.transitions]]`, …) must parse
//! and round-trip here before SW-17 models those sub-tables, rather than being
//! refused over a field set that has not landed. SW-17 adds the typed fields; the
//! presence ⇒ program-audio-default-on semantics do not change.

use serde::{Deserialize, Serialize};

/// The presence-root of the `[switcher]` config block (ADR-M012 §1).
///
/// Absent ⇒ no switcher is declared. Present ⇒ the document declares a switcher,
/// which is what makes program audio default-on (ADR-0059 §8); the full switcher
/// domain schema lands with SW-17.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SwitcherConfig {}
