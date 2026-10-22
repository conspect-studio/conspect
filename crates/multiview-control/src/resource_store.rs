//! Generic versioned-document persistence for the simple management resources
//! (sources, outputs, overlays, probes).
//!
//! These resources mirror **layouts** exactly: each is stored as an
//! opaque, validated document (`id` + `name` + `body`) carrying a monotonic
//! [`Version`] for `ETag`/`If-Match` optimistic concurrency (ADR-W006). The
//! `body` is the config-as-code shape (`multiview_config::Source` / `Output` /
//! `Overlay`) serialised to canonical JSON; the control plane stores and
//! version-stamps it without interpreting its internals here (engine-side
//! validation against `multiview-config` happens before it is applied).
//!
//! The config types live in the FFI-free `multiview-config` crate and carry no
//! `utoipa::ToSchema` derive, so — exactly like the config-versioning store
//! ([`crate::versioning`]) — the body is held as a [`serde_json::Value`] rather
//! than deriving a web schema on a foreign type.
//!
//! The trait + store are parameterised by a [`ResourceKind`] so one tested
//! implementation backs all three resources. Like every other control-plane
//! store, the lock guards only control-plane state and is never held by the
//! engine, so it cannot back-pressure the engine (invariant #10).
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::concurrency::Version;
use crate::error::{ControlError, ControlResult};

/// The resource collection name used in problem documents and not-found errors
/// for the `sources` resource.
pub const SOURCE_KIND: &str = "source";
/// The resource collection name used in problem documents and not-found errors
/// for the `outputs` resource.
pub const OUTPUT_KIND: &str = "output";
/// The resource collection name used in problem documents and not-found errors
/// for the `overlays` resource.
pub const OVERLAY_KIND: &str = "overlay";
/// The resource collection name used in problem documents and not-found errors
/// for the `probes` resource.
pub const PROBE_KIND: &str = "probe";
/// The resource collection name used in problem documents and not-found errors
/// for the `devices` resource (managed-device registry, ADR-M008).
pub const DEVICE_KIND: &str = "device";
/// The resource collection name used in problem documents and not-found errors
/// for the `sync-groups` resource (presentation-sync groups, ADR-M008/M010).
pub const SYNC_GROUP_KIND: &str = "sync-group";
/// The resource collection name used in problem documents and not-found errors
/// for the `media-players` resource (pre-declared bus-selectable player
/// channels, ADR-0057 / ADR-0097).
pub const MEDIA_PLAYER_KIND: &str = "media-player";

/// A marker selecting which resource collection a store serves, supplying the
/// stable kind name used in errors and audit records.
pub trait ResourceKind: Send + Sync + 'static {
    /// The collection name (e.g. `"source"`).
    const KIND: &'static str;
}

/// The `sources` resource marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceKind;
impl ResourceKind for SourceKind {
    const KIND: &'static str = SOURCE_KIND;
}

/// The `outputs` resource marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputKind;
impl ResourceKind for OutputKind {
    const KIND: &'static str = OUTPUT_KIND;
}

/// The `overlays` resource marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayKind;
impl ResourceKind for OverlayKind {
    const KIND: &'static str = OVERLAY_KIND;
}

/// The `probes` resource marker (per-cell fail-state detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeKind;
impl ResourceKind for ProbeKind {
    const KIND: &'static str = PROBE_KIND;
}

/// The `devices` resource marker (managed-device registry, ADR-M008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceKind;
impl ResourceKind for DeviceKind {
    const KIND: &'static str = DEVICE_KIND;
}

/// The `sync-groups` resource marker (presentation-sync groups, ADR-M008/M010).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncGroupKind;
impl ResourceKind for SyncGroupKind {
    const KIND: &'static str = SYNC_GROUP_KIND;
}

/// The `media-players` resource marker (pre-declared bus-selectable player
/// channels, ADR-0057 / ADR-0097).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPlayerKind;
impl ResourceKind for MediaPlayerKind {
    const KIND: &'static str = MEDIA_PLAYER_KIND;
}

/// A persisted management resource: a stable `id`, a display `name`, and the
/// opaque `body` document (the config-as-code shape as canonical JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Resource {
    /// Stable resource id.
    pub id: String,
    /// Human-friendly name.
    pub name: String,
    /// The opaque resource document, as canonical JSON.
    pub body: serde_json::Value,
}

/// A resource together with its current optimistic-concurrency version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedResource {
    /// The version that stamps the `ETag` and is matched by `If-Match`.
    pub version: Version,
    /// The resource payload.
    pub resource: Resource,
}

/// The fields a create/update accepts (the id is supplied separately on create
/// and immutable on update).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResourceInput {
    /// Human-friendly name. Optional on the wire: a create/update that omits it
    /// (e.g. a programmatic seed that only carries the typed `body`) defaults to
    /// an empty name rather than failing deserialization — the display name is a
    /// label, never load-bearing, and callers that care always supply one.
    #[serde(default)]
    pub name: String,
    /// The opaque resource document.
    pub body: serde_json::Value,
}

/// CRUD persistence for one simple management resource collection.
///
/// Implementations are control-plane state only and **never** sit on the
/// engine's data plane, so however they synchronise internally they cannot
/// back-pressure the engine (invariant #10).
pub trait ResourceRepository: Send + Sync + 'static {
    /// The collection name used in not-found errors and audit records.
    fn kind(&self) -> &'static str;

    /// List all resources (in a stable, id-sorted order).
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn list(&self) -> ControlResult<Vec<VersionedResource>>;

    /// Fetch one resource by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no resource has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn get(&self, id: &str) -> ControlResult<VersionedResource>;

    /// Create a resource with the given id, returning it at
    /// [`Version::INITIAL`].
    ///
    /// # Errors
    ///
    /// [`ControlError::Validation`] if the id already exists;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn create(&self, id: &str, input: ResourceInput) -> ControlResult<VersionedResource>;

    /// Replace a resource's payload, bumping its version.
    ///
    /// The caller is responsible for having already enforced any `If-Match`
    /// precondition against [`ResourceRepository::get`]'s version.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no resource has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn update(&self, id: &str, input: ResourceInput) -> ControlResult<VersionedResource>;

    /// Delete a resource by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no resource has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn delete(&self, id: &str) -> ControlResult<()>;
}

/// An in-memory [`ResourceRepository`] backed by a `Mutex<HashMap>`, generic
/// over the [`ResourceKind`] it serves.
///
/// This is the default, fully-tested store. Its lock guards only control-plane
/// state — it is never held by the engine — so it cannot back-pressure the
/// engine.
#[derive(Debug)]
pub struct InMemoryResourceStore<K: ResourceKind> {
    items: Mutex<HashMap<String, VersionedResource>>,
    _kind: PhantomData<fn() -> K>,
}

/// An in-memory `sources` store.
pub type InMemorySourceStore = InMemoryResourceStore<SourceKind>;
/// An in-memory `outputs` store.
pub type InMemoryOutputStore = InMemoryResourceStore<OutputKind>;
/// An in-memory `overlays` store.
pub type InMemoryOverlayStore = InMemoryResourceStore<OverlayKind>;
/// An in-memory `probes` store.
pub type InMemoryProbeStore = InMemoryResourceStore<ProbeKind>;
/// An in-memory `devices` store (the managed-device registry, ADR-M008).
pub type InMemoryDeviceStore = InMemoryResourceStore<DeviceKind>;
/// An in-memory `sync-groups` store (presentation-sync groups, ADR-M008/M010).
pub type InMemorySyncGroupStore = InMemoryResourceStore<SyncGroupKind>;
/// An in-memory `media-players` store (player channels, ADR-0057 / ADR-0097).
pub type InMemoryMediaPlayerStore = InMemoryResourceStore<MediaPlayerKind>;

impl<K: ResourceKind> Default for InMemoryResourceStore<K> {
    fn default() -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
            _kind: PhantomData,
        }
    }
}

impl<K: ResourceKind> InMemoryResourceStore<K> {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VersionedResource>> {
        match self.items.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl<K: ResourceKind> ResourceRepository for InMemoryResourceStore<K> {
    fn kind(&self) -> &'static str {
        K::KIND
    }

    fn list(&self) -> ControlResult<Vec<VersionedResource>> {
        let guard = self.lock();
        let mut out: Vec<VersionedResource> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.resource.id.cmp(&b.resource.id));
        Ok(out)
    }

    fn get(&self, id: &str) -> ControlResult<VersionedResource> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound {
                kind: K::KIND,
                id: id.to_owned(),
            })
    }

    fn create(&self, id: &str, input: ResourceInput) -> ControlResult<VersionedResource> {
        let mut guard = self.lock();
        if guard.contains_key(id) {
            return Err(ControlError::Validation(format!(
                "{} {id:?} already exists",
                K::KIND
            )));
        }
        let versioned = VersionedResource {
            version: Version::INITIAL,
            resource: Resource {
                id: id.to_owned(),
                name: input.name,
                body: input.body,
            },
        };
        guard.insert(id.to_owned(), versioned.clone());
        Ok(versioned)
    }

    fn update(&self, id: &str, input: ResourceInput) -> ControlResult<VersionedResource> {
        let mut guard = self.lock();
        let existing = guard.get(id).ok_or_else(|| ControlError::NotFound {
            kind: K::KIND,
            id: id.to_owned(),
        })?;
        let next = VersionedResource {
            version: existing.version.next(),
            resource: Resource {
                id: id.to_owned(),
                name: input.name,
                body: input.body,
            },
        };
        guard.insert(id.to_owned(), next.clone());
        Ok(next)
    }

    fn delete(&self, id: &str) -> ControlResult<()> {
        let mut guard = self.lock();
        if guard.remove(id).is_some() {
            Ok(())
        } else {
            Err(ControlError::NotFound {
                kind: K::KIND,
                id: id.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use serde_json::json;

    use super::{
        InMemoryDeviceStore, InMemoryOutputStore, InMemoryOverlayStore, InMemoryProbeStore,
        InMemorySourceStore, InMemorySyncGroupStore, ResourceInput, ResourceRepository,
        DEVICE_KIND, OUTPUT_KIND, OVERLAY_KIND, PROBE_KIND, SOURCE_KIND, SYNC_GROUP_KIND,
    };
    use crate::concurrency::Version;
    use crate::error::ControlError;

    fn input(name: &str) -> ResourceInput {
        ResourceInput {
            name: name.to_owned(),
            body: json!({ "kind": "test", "value": name }),
        }
    }

    #[test]
    fn create_then_get_round_trips_at_initial_version() {
        let store = InMemorySourceStore::new();
        let v = store.create("s1", input("Cam 1")).unwrap();
        assert_eq!(v.version, Version::INITIAL);
        assert_eq!(v.resource.id, "s1");
        assert_eq!(v.resource.name, "Cam 1");
        let got = store.get("s1").unwrap();
        assert_eq!(got, v);
    }

    #[test]
    fn duplicate_create_is_a_validation_error() {
        let store = InMemoryOutputStore::new();
        store.create("o1", input("Main")).unwrap();
        let err = store.create("o1", input("Main")).unwrap_err();
        assert!(matches!(err, ControlError::Validation(_)));
    }

    #[test]
    fn update_bumps_version_and_replaces_body() {
        let store = InMemoryOverlayStore::new();
        store.create("ov1", input("Clock")).unwrap();
        let v2 = store.update("ov1", input("Clock 2")).unwrap();
        assert_eq!(v2.version, Version::INITIAL.next());
        assert_eq!(v2.resource.name, "Clock 2");
    }

    #[test]
    fn update_unknown_is_not_found() {
        let store = InMemorySourceStore::new();
        let err = store.update("missing", input("x")).unwrap_err();
        assert!(matches!(
            err,
            ControlError::NotFound {
                kind: SOURCE_KIND,
                ..
            }
        ));
    }

    #[test]
    fn list_is_id_sorted_and_delete_removes() {
        let store = InMemorySourceStore::new();
        store.create("zeta", input("Z")).unwrap();
        store.create("alpha", input("A")).unwrap();
        let listed = store.list().unwrap();
        let ids: Vec<&str> = listed.iter().map(|v| v.resource.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);

        store.delete("alpha").unwrap();
        assert!(store.get("alpha").is_err());
        assert_eq!(store.list().unwrap().len(), 1);

        let err = store.delete("alpha").unwrap_err();
        assert!(matches!(err, ControlError::NotFound { .. }));
    }

    #[test]
    fn kind_reports_the_collection_name() {
        assert_eq!(InMemorySourceStore::new().kind(), SOURCE_KIND);
        assert_eq!(InMemoryOutputStore::new().kind(), OUTPUT_KIND);
        assert_eq!(InMemoryOverlayStore::new().kind(), OVERLAY_KIND);
        assert_eq!(InMemoryProbeStore::new().kind(), PROBE_KIND);
        assert_eq!(InMemoryDeviceStore::new().kind(), DEVICE_KIND);
        assert_eq!(InMemorySyncGroupStore::new().kind(), SYNC_GROUP_KIND);
    }
}
