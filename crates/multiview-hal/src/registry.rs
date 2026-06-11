//! The backend registry: register and query capabilities by `(stage, kind)`.
//!
//! The registry is the planner's view of *what exists* on this host. Capability
//! detection (pure-Rust defaults plus the feature-gated probes in
//! [`crate::probe`]) populates it; the planner queries it. It deliberately
//! holds plain [`Capability`] descriptors, not `Box<dyn Backend>` — the planner
//! reasons over capabilities and costs, and the concrete backend objects live
//! in the feature-gated crates.
use std::collections::BTreeMap;

use multiview_core::traits::BackendKind;

use crate::capability::{Capability, Stage};
use crate::error::{Error, Result};

/// A stable, total ordinal for a [`BackendKind`].
///
/// `multiview_core::BackendKind` deliberately derives only `PartialEq`/`Eq` (it is
/// `#[non_exhaustive]`), so it is neither `Ord` nor `Hash`. The registry needs a
/// deterministic key, so we assign each variant a fixed ordinal here. The
/// wildcard arm keeps this total across future `#[non_exhaustive]` additions;
/// new kinds simply sort after the known ones (and the registry still works —
/// determinism, not a specific order, is what matters).
const fn kind_ordinal(kind: BackendKind) -> u8 {
    match kind {
        BackendKind::Software => 0,
        BackendKind::Cuda => 1,
        BackendKind::VideoToolbox => 2,
        BackendKind::Vaapi => 3,
        BackendKind::Qsv => 4,
        BackendKind::Wgpu => 5,
        BackendKind::Metal => 6,
        _ => u8::MAX,
    }
}

/// A registry of backend capabilities, keyed by `(stage, kind)`.
///
/// Lookups are deterministic and ordered (a `BTreeMap` keyed by stage plus a
/// stable backend-kind ordinal under the hood), so planning is reproducible
/// regardless of registration order. The full [`Capability`] (which carries its
/// own `kind`) is stored as the value, so no information is lost to the ordinal.
#[derive(Debug, Clone, Default)]
pub struct BackendRegistry {
    entries: BTreeMap<(Stage, u8), Capability>,
}

impl BackendRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Register a backend capability.
    ///
    /// The capability is validated ([`Capability::validate`]) before insertion.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if the descriptor is malformed, or
    /// [`Error::DuplicateBackend`] if a capability for the same `(stage, kind)`
    /// is already registered (use [`Self::replace`] to overwrite deliberately).
    pub fn register(&mut self, capability: Capability) -> Result<()> {
        capability.validate()?;
        let key = (capability.stage, kind_ordinal(capability.kind));
        if self.entries.contains_key(&key) {
            return Err(Error::DuplicateBackend {
                stage: capability.stage,
                kind: capability.kind,
            });
        }
        self.entries.insert(key, capability);
        Ok(())
    }

    /// Register or overwrite a backend capability.
    ///
    /// Returns the previously registered capability for this `(stage, kind)`,
    /// if any.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if the descriptor is malformed.
    pub fn replace(&mut self, capability: Capability) -> Result<Option<Capability>> {
        capability.validate()?;
        let key = (capability.stage, kind_ordinal(capability.kind));
        Ok(self.entries.insert(key, capability))
    }

    /// Look up the capability for a specific `(stage, kind)`.
    #[must_use]
    pub fn get(&self, stage: Stage, kind: BackendKind) -> Option<&Capability> {
        self.entries.get(&(stage, kind_ordinal(kind)))
    }

    /// Look up the capability for a `(stage, kind)`, returning an error when
    /// absent (the fallible form used by the planner).
    ///
    /// # Errors
    ///
    /// Returns [`Error::BackendNotFound`] when no capability is registered.
    pub fn require(&self, stage: Stage, kind: BackendKind) -> Result<&Capability> {
        self.get(stage, kind)
            .ok_or(Error::BackendNotFound { stage, kind })
    }

    /// Whether a capability exists for `(stage, kind)`.
    #[must_use]
    pub fn contains(&self, stage: Stage, kind: BackendKind) -> bool {
        self.entries.contains_key(&(stage, kind_ordinal(kind)))
    }

    /// All capabilities registered for `stage`, ordered by [`BackendKind`].
    #[must_use]
    pub fn for_stage(&self, stage: Stage) -> Vec<&Capability> {
        self.entries
            .iter()
            .filter(|((s, _), _)| *s == stage)
            .map(|(_, capability)| capability)
            .collect()
    }

    /// Number of registered capabilities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no capabilities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A registry pre-populated with the always-available software backend for
    /// every stage (the universal fallback tier; invariant #9 keeps software as
    /// the floor the planner can always drop to).
    #[must_use]
    pub fn with_software_defaults() -> Self {
        let mut registry = Self::new();
        for stage in Stage::ALL {
            let capability = crate::probe::software_capability(stage);
            // Software defaults are constructed by us and are always valid;
            // `replace` cannot return a duplicate error, and a malformed
            // built-in would be a bug surfaced by the unit tests rather than a
            // runtime path. Use `replace` so this is total.
            let _ = registry.replace(capability);
        }
        registry
    }
}
