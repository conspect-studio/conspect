//! The **salvo** engine: an atomic multi-change recall with arm → take → cancel
//! semantics (broadcast-multiviewer brief §8, ADR-MV001).
//!
//! A salvo is the broadcast operator's "recall preset": a named, pre-built set of
//! changes — a layout swap, source rebinds, tally overrides, UMD label updates —
//! that **all apply together or not at all**. The operator *arms* the salvo (loads
//! it, pending), reviews it, then *takes* it (commits) at a clean boundary, or
//! *cancels* it. The engine applies the returned [`SalvoBatch`] as one Class-1 hot
//! reconfiguration at a frame boundary.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! This is a pure value machine: [`Salvo::arm`] / [`Salvo::take`] / [`Salvo::cancel`]
//! mutate only the salvo's own phase and **return** the command batch for the
//! engine to apply. Nothing here reaches into the engine, blocks, or `.await`s.
//! `take` is **all-or-nothing** (it returns the whole batch or, when not armed,
//! [`None`] and applies nothing) and **idempotent** (a second `take` of the same
//! armed-then-taken salvo returns [`None`], not a double-apply).
use serde::{Deserialize, Serialize};

/// One change within a salvo batch.
///
/// The variants cover the four change classes a preset recall touches. They are
/// declarative *requests* the engine applies atomically at a frame boundary, not
/// mutations performed here. Serialised **tagged** (`#[serde(tag = "kind")]`) per
/// repo conventions; never `untagged`. `#[non_exhaustive]` for forward
/// compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SalvoChange {
    /// Swap the active layout (by name) on a head.
    Layout {
        /// Target head id.
        head: String,
        /// Name of the [`Layout`](multiview_core::layout::Layout) to make active.
        layout: String,
    },
    /// Bind a source to a tile.
    SourceBind {
        /// Zero-based tile index.
        tile: u32,
        /// Source id to bind (`None` clears the binding).
        source: Option<String>,
    },
    /// Override a tile's tally state.
    Tally {
        /// Zero-based tile index.
        tile: u32,
        /// The lamp colour to force (TSL palette code via
        /// [`TallyColor`](multiview_core::tally::TallyColor)).
        color: multiview_core::tally::TallyColor,
    },
    /// Set a tile's under-monitor-display label.
    Umd {
        /// Zero-based tile index.
        tile: u32,
        /// The UMD text to display.
        text: String,
    },
}

/// An atomic batch of changes a salvo applies on take.
///
/// Returned by [`Salvo::take`] for the engine to apply as **one** frame-boundary
/// reconfiguration. The order of [`changes`](SalvoBatch::changes) is the salvo's
/// declaration order, which the engine applies as a single transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SalvoBatch {
    /// The salvo's name (for events / audit).
    pub name: String,
    /// The changes, in declaration order, applied atomically.
    pub changes: Vec<SalvoChange>,
}

impl SalvoBatch {
    /// Whether the batch is empty (no changes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The number of changes in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }
}

/// The lifecycle phase of a [`Salvo`].
///
/// `Idle` → `Armed` (loaded, pending review) → `Taken` (committed) or back to
/// `Idle` on cancel. `#[non_exhaustive]` for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SalvoPhase {
    /// Not armed; a take is a no-op.
    #[default]
    Idle,
    /// Armed and pending: a take will commit the batch.
    Armed,
    /// Already taken: a further take is a no-op (idempotent) until re-armed.
    Taken,
}

/// A named, atomic preset recall with arm → take → cancel semantics.
///
/// Construct with [`Salvo::new`] (defining its changes), then drive its
/// lifecycle. The salvo carries its phase only; the changes are immutable once
/// constructed. `Clone` for lock-free snapshotting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Salvo {
    name: String,
    changes: Vec<SalvoChange>,
    phase: SalvoPhase,
}

impl Salvo {
    /// Construct an idle salvo named `name` with the given ordered `changes`.
    #[must_use]
    pub fn new(name: impl Into<String>, changes: Vec<SalvoChange>) -> Self {
        Self {
            name: name.into(),
            changes,
            phase: SalvoPhase::Idle,
        }
    }

    /// The salvo's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The salvo's declared changes (immutable).
    #[must_use]
    pub fn changes(&self) -> &[SalvoChange] {
        &self.changes
    }

    /// The current lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> SalvoPhase {
        self.phase
    }

    /// Whether the salvo is currently armed (a take will commit).
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        matches!(self.phase, SalvoPhase::Armed)
    }

    /// Arm the salvo: move `Idle`/`Taken` → `Armed` so a subsequent
    /// [`take`](Salvo::take) commits. Idempotent — arming an already-armed salvo
    /// is a no-op. Returns `true` if the phase changed to `Armed` this call (it
    /// was not already armed).
    pub fn arm(&mut self) -> bool {
        if matches!(self.phase, SalvoPhase::Armed) {
            false
        } else {
            self.phase = SalvoPhase::Armed;
            true
        }
    }

    /// Cancel an armed salvo: move `Armed` → `Idle`, applying nothing. A no-op
    /// (returns `false`) when not armed. Idempotent.
    pub fn cancel(&mut self) -> bool {
        if matches!(self.phase, SalvoPhase::Armed) {
            self.phase = SalvoPhase::Idle;
            true
        } else {
            false
        }
    }

    /// Take (commit) an armed salvo, **returning the whole [`SalvoBatch`]** for
    /// the engine to apply atomically, and moving `Armed` → `Taken`.
    ///
    /// All-or-nothing and idempotent:
    ///
    /// * armed → returns `Some(batch)` exactly once and becomes `Taken`;
    /// * not armed (idle or already taken) → returns [`None`] and changes
    ///   nothing — a repeated take never double-applies.
    ///
    /// The returned batch is a *clone* of the salvo's immutable changes, so the
    /// engine applies a self-contained transaction.
    pub fn take(&mut self) -> Option<SalvoBatch> {
        if matches!(self.phase, SalvoPhase::Armed) {
            self.phase = SalvoPhase::Taken;
            Some(SalvoBatch {
                name: self.name.clone(),
                changes: self.changes.clone(),
            })
        } else {
            None
        }
    }
}
