//! Salvo persistence: the [`SalvoRepository`] trait and its in-memory default.
//!
//! A **salvo** is a named, atomically-applied recall (recall a layout, rebind
//! sources, force tally, set UMD text) the operator stages with **arm** and
//! fires with **take** (broadcast-multiviewer brief §8). The control plane owns
//! the declarative definitions (the config-as-code [`multiview_config::Salvo`]
//! shape) and the operator CRUD over them; the *execution* of an arm/take lives
//! in `multiview-engine` and is reached only through the bounded, non-blocking
//! command bus ([`crate::command`]) — nothing here sits on the engine's data
//! plane (invariant #10).
//!
//! Each stored salvo carries a monotonic [`Version`] for `ETag`/`If-Match`
//! optimistic concurrency (ADR-W006), exactly like layouts and alarms.
//!
//! The **tested default** is [`InMemorySalvoStore`] (pure Rust, no native deps).
use std::collections::HashMap;
use std::sync::Mutex;

use multiview_config::Salvo;

use crate::concurrency::Version;
use crate::error::{ControlError, ControlResult};

/// The resource collection name used in problem documents and not-found errors.
pub const SALVO_KIND: &str = "salvo";

/// A stored salvo definition together with its optimistic-concurrency version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedSalvo {
    /// The version that stamps the `ETag` and is matched by `If-Match`.
    pub version: Version,
    /// The salvo definition.
    pub salvo: Salvo,
}

/// Versioned persistence for the control plane's salvo definitions.
///
/// Implementations are control-plane state only and **never** sit on the
/// engine's data plane, so however they synchronise internally they cannot
/// back-pressure the engine (invariant #10).
pub trait SalvoRepository: Send + Sync + 'static {
    /// List all salvos in a stable, id-sorted order.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn list(&self) -> ControlResult<Vec<VersionedSalvo>>;

    /// Fetch one salvo by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no salvo has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn get(&self, id: &str) -> ControlResult<VersionedSalvo>;

    /// Create a salvo, returning it at [`Version::INITIAL`].
    ///
    /// The `salvo`'s own [`Salvo::validate`] is enforced by the caller before
    /// this is reached. The store rejects a duplicate id.
    ///
    /// # Errors
    ///
    /// [`ControlError::Validation`] if the id already exists;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn create(&self, salvo: Salvo) -> ControlResult<VersionedSalvo>;

    /// Replace a salvo's definition, bumping its version.
    ///
    /// The caller is responsible for having already enforced any `If-Match`
    /// precondition against [`SalvoRepository::get`]'s version.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no salvo has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn update(&self, id: &str, salvo: Salvo) -> ControlResult<VersionedSalvo>;

    /// Delete a salvo by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no salvo has that id;
    /// [`ControlError::Repository`] on a backing-store fault.
    fn delete(&self, id: &str) -> ControlResult<()>;
}

/// An in-memory [`SalvoRepository`] backed by a `Mutex<HashMap>`.
///
/// The default, fully-tested store. Its lock guards only control-plane state —
/// it is never held by the engine — so it cannot back-pressure the engine.
#[derive(Debug, Default)]
pub struct InMemorySalvoStore {
    salvos: Mutex<HashMap<String, VersionedSalvo>>,
}

impl InMemorySalvoStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VersionedSalvo>> {
        match self.salvos.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl SalvoRepository for InMemorySalvoStore {
    fn list(&self) -> ControlResult<Vec<VersionedSalvo>> {
        let guard = self.lock();
        let mut out: Vec<VersionedSalvo> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.salvo.id.cmp(&b.salvo.id));
        Ok(out)
    }

    fn get(&self, id: &str) -> ControlResult<VersionedSalvo> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound {
                kind: SALVO_KIND,
                id: id.to_owned(),
            })
    }

    fn create(&self, salvo: Salvo) -> ControlResult<VersionedSalvo> {
        let mut guard = self.lock();
        if guard.contains_key(&salvo.id) {
            return Err(ControlError::Validation(format!(
                "salvo {:?} already exists",
                salvo.id
            )));
        }
        let versioned = VersionedSalvo {
            version: Version::INITIAL,
            salvo,
        };
        guard.insert(versioned.salvo.id.clone(), versioned.clone());
        Ok(versioned)
    }

    fn update(&self, id: &str, salvo: Salvo) -> ControlResult<VersionedSalvo> {
        let mut guard = self.lock();
        let existing = guard.get(id).ok_or_else(|| ControlError::NotFound {
            kind: SALVO_KIND,
            id: id.to_owned(),
        })?;
        let next = VersionedSalvo {
            version: existing.version.next(),
            salvo,
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
                kind: SALVO_KIND,
                id: id.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_config::Salvo;

    use super::{InMemorySalvoStore, SalvoRepository, SALVO_KIND};
    use crate::concurrency::Version;
    use crate::error::ControlError;

    // The config types are `#[non_exhaustive]`, so construct them via serde
    // (their public, tagged wire form) rather than struct literals.
    fn salvo(id: &str) -> Salvo {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "layout": "wide",
            "tally": [{ "cell": "c0", "color": "Red" }],
        }))
        .expect("salvo deserialises")
    }

    #[test]
    fn create_then_get_round_trips_at_initial_version() {
        let store = InMemorySalvoStore::new();
        let v = store.create(salvo("s1")).unwrap();
        assert_eq!(v.version, Version::INITIAL);
        let got = store.get("s1").unwrap();
        assert_eq!(got, v);
    }

    #[test]
    fn duplicate_create_is_a_validation_error() {
        let store = InMemorySalvoStore::new();
        store.create(salvo("s1")).unwrap();
        let err = store.create(salvo("s1")).unwrap_err();
        assert!(matches!(err, ControlError::Validation(_)));
    }

    #[test]
    fn update_bumps_version_and_replaces_body() {
        let store = InMemorySalvoStore::new();
        store.create(salvo("s1")).unwrap();
        let mut changed = salvo("s1");
        changed.display_name = Some("Wide shot".to_owned());
        let v2 = store.update("s1", changed).unwrap();
        assert_eq!(v2.version, Version::INITIAL.next());
        assert_eq!(v2.salvo.display_name.as_deref(), Some("Wide shot"));
    }

    #[test]
    fn update_unknown_is_not_found() {
        let store = InMemorySalvoStore::new();
        let err = store.update("missing", salvo("missing")).unwrap_err();
        assert!(matches!(
            err,
            ControlError::NotFound {
                kind: SALVO_KIND,
                ..
            }
        ));
    }

    #[test]
    fn list_is_id_sorted_and_delete_removes() {
        let store = InMemorySalvoStore::new();
        store.create(salvo("zeta")).unwrap();
        store.create(salvo("alpha")).unwrap();
        let listed = store.list().unwrap();
        let ids: Vec<&str> = listed.iter().map(|v| v.salvo.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);

        store.delete("alpha").unwrap();
        assert!(store.get("alpha").is_err());
        assert_eq!(store.list().unwrap().len(), 1);

        let err = store.delete("alpha").unwrap_err();
        assert!(matches!(err, ControlError::NotFound { .. }));
    }
}
