//! The resource [`Repository`] trait and its in-memory default implementation.
//!
//! The control plane persists management resources (layouts, sources, …) behind
//! this trait so the HTTP handlers never touch a concrete store directly. The
//! **tested default** is [`InMemoryRepository`] (pure Rust, no native deps); an
//! `sqlx`/SQLite-backed implementation lives behind the off-by-default `sqlite`
//! module (`SQLite`'s license is outside the cargo-deny allowlist, so it must
//! never be in the default build).
//!
//! Each stored resource carries a monotonic [`Version`] for `ETag`/`If-Match`
//! optimistic concurrency (ADR-W006).
use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::concurrency::Version;
use crate::error::{ControlError, ControlResult};

/// The resource collection name used in problem documents and not-found errors.
pub const LAYOUT_KIND: &str = "layout";

/// A persisted layout resource.
///
/// The `body` is the opaque, validated layout document the editor produces; the
/// control plane stores and version-stamps it without interpreting its
/// internals here. (Engine-side validation against `multiview-config` happens
/// before a layout is applied.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Layout {
    /// Stable resource id.
    pub id: String,
    /// Human-friendly name.
    pub name: String,
    /// The opaque layout document (canvas + cells), as canonical JSON.
    pub body: serde_json::Value,
}

/// A layout together with its current optimistic-concurrency version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedLayout {
    /// The version that stamps the `ETag` and is matched by `If-Match`.
    pub version: Version,
    /// The layout payload.
    pub layout: Layout,
}

/// The fields a create/update accepts (the id is supplied separately on create
/// and immutable on update).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LayoutInput {
    /// Human-friendly name.
    pub name: String,
    /// The opaque layout document.
    pub body: serde_json::Value,
}

/// CRUD persistence for the control plane's management resources.
///
/// Implementations are control-plane state only and **never** sit on the
/// engine's data plane, so however they synchronize internally they cannot
/// back-pressure the engine (invariant #10).
pub trait Repository: Send + Sync + 'static {
    /// List all layouts (in a stable, id-sorted order).
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn list_layouts(&self) -> ControlResult<Vec<VersionedLayout>>;

    /// Fetch one layout by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no layout has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn get_layout(&self, id: &str) -> ControlResult<VersionedLayout>;

    /// Create a layout with the given id, returning it at
    /// [`Version::INITIAL`].
    ///
    /// # Errors
    ///
    /// [`ControlError::Validation`] if the id already exists;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn create_layout(&self, id: &str, input: LayoutInput) -> ControlResult<VersionedLayout>;

    /// Replace a layout's payload, bumping its version.
    ///
    /// The caller is responsible for having already enforced any `If-Match`
    /// precondition against [`Repository::get_layout`]'s version.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no layout has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn update_layout(&self, id: &str, input: LayoutInput) -> ControlResult<VersionedLayout>;

    /// Delete a layout by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no layout has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn delete_layout(&self, id: &str) -> ControlResult<()>;
}

/// An in-memory [`Repository`] backed by a `Mutex<HashMap>`.
///
/// This is the default, fully-tested store. Its lock guards only control-plane
/// state — it is never held by the engine — so it cannot back-pressure the
/// engine.
#[derive(Debug, Default)]
pub struct InMemoryRepository {
    layouts: Mutex<HashMap<String, VersionedLayout>>,
}

impl InMemoryRepository {
    /// A fresh, empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VersionedLayout>> {
        match self.layouts.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Repository for InMemoryRepository {
    fn list_layouts(&self) -> ControlResult<Vec<VersionedLayout>> {
        let guard = self.lock();
        let mut out: Vec<VersionedLayout> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.layout.id.cmp(&b.layout.id));
        Ok(out)
    }

    fn get_layout(&self, id: &str) -> ControlResult<VersionedLayout> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound {
                kind: LAYOUT_KIND,
                id: id.to_owned(),
            })
    }

    fn create_layout(&self, id: &str, input: LayoutInput) -> ControlResult<VersionedLayout> {
        let mut guard = self.lock();
        if guard.contains_key(id) {
            return Err(ControlError::Validation(format!(
                "layout {id:?} already exists"
            )));
        }
        let versioned = VersionedLayout {
            version: Version::INITIAL,
            layout: Layout {
                id: id.to_owned(),
                name: input.name,
                body: input.body,
            },
        };
        guard.insert(id.to_owned(), versioned.clone());
        Ok(versioned)
    }

    fn update_layout(&self, id: &str, input: LayoutInput) -> ControlResult<VersionedLayout> {
        let mut guard = self.lock();
        let existing = guard.get(id).ok_or_else(|| ControlError::NotFound {
            kind: LAYOUT_KIND,
            id: id.to_owned(),
        })?;
        let next = VersionedLayout {
            version: existing.version.next(),
            layout: Layout {
                id: id.to_owned(),
                name: input.name,
                body: input.body,
            },
        };
        guard.insert(id.to_owned(), next.clone());
        Ok(next)
    }

    fn delete_layout(&self, id: &str) -> ControlResult<()> {
        let mut guard = self.lock();
        if guard.remove(id).is_some() {
            Ok(())
        } else {
            Err(ControlError::NotFound {
                kind: LAYOUT_KIND,
                id: id.to_owned(),
            })
        }
    }
}
