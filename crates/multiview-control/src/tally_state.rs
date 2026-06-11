//! The control plane's tally mirror, manual-override registry, and tally-profile
//! store.
//!
//! Three control-only concerns live here, all isolation-safe (invariant #10 —
//! none sits on the engine's data plane):
//!
//! * **The tally mirror** ([`TallyMirror`]): a latest-wins snapshot of each
//!   tile/element's resolved tally lamp state, fed lossily from the engine event
//!   stream so the REST API can answer "what is lit right now?". The ingest is a
//!   read-only, lagged-skip subscriber ([`crate::tally_ingest`]); a missed
//!   intermediate state is safe because the next [`TallyEvent`] for that target
//!   carries the current value.
//! * **The manual-override registry** ([`OverrideRegistry`]): the operator's
//!   forced-lamp requests the control plane has *submitted* to the engine, kept
//!   so the API can report "this element is under a manual override". The
//!   override is *applied* by the engine's arbiter; this is the control-plane
//!   record of the request.
//! * **The tally-profile store** ([`TallyProfileRepository`]): versioned CRUD
//!   over the config-as-code [`multiview_config::TallyProfile`] (the bit↔colour and
//!   index↔cell binding for an external tally bus), with `ETag`/`If-Match`.
//!
//! The pure classifier [`tally_observation`] (which engine events carry a tally
//! state) mirrors the alarm classifier so the ingest loop is exhaustively
//! unit-testable with no async, sockets, or sleeps.
use std::collections::HashMap;
use std::sync::Mutex;

use multiview_config::TallyProfile;
use multiview_core::tally::{TallyColor, TallyState};
use multiview_events::{Event, TallyEvent, TallyTarget};
use serde::{Deserialize, Serialize};

use crate::concurrency::Version;
use crate::error::{ControlError, ControlResult};

/// The resource collection name used in problem documents and not-found errors.
pub const TALLY_PROFILE_KIND: &str = "tally_profile";

/// A stable, hashable key for a [`TallyTarget`].
///
/// [`TallyTarget`] is `#[non_exhaustive]` and does not derive `Hash`, so the
/// mirror keys its map by this flattened string form (`tile:<n>` /
/// `element:<name>`), which is total and collision-free across the two known
/// target kinds.
#[must_use]
pub fn target_key(target: &TallyTarget) -> String {
    match target {
        TallyTarget::Tile { index } => format!("tile:{index}"),
        TallyTarget::Element { name } => format!("element:{name}"),
        // `TallyTarget` is `#[non_exhaustive]`; an unknown future target maps to
        // a generic, still-unique key rather than failing the build.
        _ => "other:unknown".to_owned(),
    }
}

/// One resolved tally entry: the target and its current lamp state.
///
/// This is the wire form the read endpoint returns. It is serde-serialisable so
/// the REST list returns it directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyEntry {
    /// What the tally state applies to.
    pub target: TallyTarget,
    /// The resolved tally lamp state.
    pub state: TallyState,
}

impl From<TallyEvent> for TallyEntry {
    fn from(event: TallyEvent) -> Self {
        Self {
            target: event.target,
            state: event.state,
        }
    }
}

/// A latest-wins snapshot of each target's resolved tally state.
///
/// Fed by the engine event stream (lossily, lagged-skip) and read by the REST
/// API. Control-plane state only; its lock is never held by the engine.
#[derive(Debug, Default)]
pub struct TallyMirror {
    entries: Mutex<HashMap<String, TallyEntry>>,
}

impl TallyMirror {
    /// A fresh, empty mirror.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TallyEntry>> {
        match self.entries.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Record the latest resolved tally state for a target (overwriting any
    /// prior state for the same target — latest wins).
    pub fn apply(&self, event: TallyEvent) {
        let key = target_key(&event.target);
        self.lock().insert(key, TallyEntry::from(event));
    }

    /// The current resolved state for one target, if the mirror has seen it.
    #[must_use]
    pub fn get(&self, target: &TallyTarget) -> Option<TallyEntry> {
        self.lock().get(&target_key(target)).cloned()
    }

    /// Every resolved tally entry, in a stable key-sorted order.
    #[must_use]
    pub fn list(&self) -> Vec<TallyEntry> {
        let guard = self.lock();
        let mut keyed: Vec<(String, TallyEntry)> =
            guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.into_iter().map(|(_, v)| v).collect()
    }

    /// Number of distinct targets the mirror holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the mirror is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// Classify an engine [`Event`] as a tally state observation, if it is one.
///
/// Returns a reference to the carried [`TallyEvent`] for `tally.state`, and
/// [`None`] for every other event. Pure and total — the unit of behaviour the
/// tally ingest loop is built on.
#[must_use]
pub fn tally_observation(event: &Event) -> Option<&TallyEvent> {
    match event {
        Event::TallyState(e) => Some(e),
        _ => None,
    }
}

/// The operator's manual tally overrides, as submitted to the engine.
///
/// An override forces a target's lamp to a fixed colour regardless of the
/// arbitrated bus state until released. This registry is the control-plane
/// *record* of the operator's requests (the engine applies them); the REST API
/// reads it to report which elements are pinned. Control-plane state only.
#[derive(Debug, Default)]
pub struct OverrideRegistry {
    overrides: Mutex<HashMap<String, TallyColor>>,
}

impl OverrideRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TallyColor>> {
        match self.overrides.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Record a forced colour for a target (overwriting any prior override).
    pub fn set(&self, target: &TallyTarget, color: TallyColor) {
        self.lock().insert(target_key(target), color);
    }

    /// Clear any override on a target, returning whether one was present.
    pub fn clear(&self, target: &TallyTarget) -> bool {
        self.lock().remove(&target_key(target)).is_some()
    }

    /// The forced colour on a target, if any.
    #[must_use]
    pub fn get(&self, target: &TallyTarget) -> Option<TallyColor> {
        self.lock().get(&target_key(target)).copied()
    }

    /// Number of active overrides.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether there are no active overrides.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// A stored tally profile together with its optimistic-concurrency version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedProfile {
    /// The version that stamps the `ETag` and is matched by `If-Match`.
    pub version: Version,
    /// The tally profile definition.
    pub profile: TallyProfile,
}

/// Versioned persistence for the control plane's tally profiles.
///
/// Control-plane state only; never on the engine's data plane (invariant #10).
pub trait TallyProfileRepository: Send + Sync + 'static {
    /// List all tally profiles in a stable, id-sorted order.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn list(&self) -> ControlResult<Vec<VersionedProfile>>;

    /// Fetch one tally profile by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no profile has that id.
    fn get(&self, id: &str) -> ControlResult<VersionedProfile>;

    /// Create or replace a tally profile.
    ///
    /// A new id is stored at [`Version::INITIAL`]; an existing id has its
    /// definition replaced and its version bumped (the caller enforces any
    /// `If-Match` precondition first).
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn put(&self, profile: TallyProfile) -> ControlResult<VersionedProfile>;

    /// Delete a tally profile by id.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no profile has that id.
    fn delete(&self, id: &str) -> ControlResult<()>;
}

/// An in-memory [`TallyProfileRepository`] backed by a `Mutex<HashMap>`.
#[derive(Debug, Default)]
pub struct InMemoryProfileStore {
    profiles: Mutex<HashMap<String, VersionedProfile>>,
}

impl InMemoryProfileStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VersionedProfile>> {
        match self.profiles.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl TallyProfileRepository for InMemoryProfileStore {
    fn list(&self) -> ControlResult<Vec<VersionedProfile>> {
        let guard = self.lock();
        let mut out: Vec<VersionedProfile> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.profile.id.cmp(&b.profile.id));
        Ok(out)
    }

    fn get(&self, id: &str) -> ControlResult<VersionedProfile> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound {
                kind: TALLY_PROFILE_KIND,
                id: id.to_owned(),
            })
    }

    fn put(&self, profile: TallyProfile) -> ControlResult<VersionedProfile> {
        let mut guard = self.lock();
        let key = profile.id.clone();
        let next = match guard.get(&key) {
            Some(existing) => VersionedProfile {
                version: existing.version.next(),
                profile,
            },
            None => VersionedProfile {
                version: Version::INITIAL,
                profile,
            },
        };
        guard.insert(key, next.clone());
        Ok(next)
    }

    fn delete(&self, id: &str) -> ControlResult<()> {
        let mut guard = self.lock();
        if guard.remove(id).is_some() {
            Ok(())
        } else {
            Err(ControlError::NotFound {
                kind: TALLY_PROFILE_KIND,
                id: id.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_config::TallyProfile;
    use multiview_core::tally::{TallyColor, TallyState};
    use multiview_events::{Alert, AlertSeverity, Event, TallyEvent, TallyTarget};

    use super::{
        tally_observation, target_key, InMemoryProfileStore, OverrideRegistry, TallyMirror,
        TallyProfileRepository, TALLY_PROFILE_KIND,
    };
    use crate::concurrency::Version;
    use crate::error::ControlError;

    fn tile(index: u32) -> TallyTarget {
        TallyTarget::Tile { index }
    }

    #[test]
    fn target_key_is_distinct_per_kind() {
        assert_eq!(target_key(&tile(3)), "tile:3");
        assert_eq!(
            target_key(&TallyTarget::Element {
                name: "wall-a".to_owned()
            }),
            "element:wall-a"
        );
        assert_ne!(target_key(&tile(3)), target_key(&tile(4)));
    }

    #[test]
    fn classifier_matches_tally_state_only() {
        let ev = TallyEvent {
            target: tile(0),
            state: TallyState::program(),
        };
        assert!(tally_observation(&Event::TallyState(ev)).is_some());
        let alert = Event::AlertRaised(Alert {
            key: "k".to_owned(),
            severity: AlertSeverity::Info,
            title: "t".to_owned(),
            detail: None,
            active: true,
        });
        assert!(tally_observation(&alert).is_none());
    }

    #[test]
    fn mirror_keeps_latest_state_per_target_and_lists_sorted() {
        let mirror = TallyMirror::new();
        assert!(mirror.is_empty());

        mirror.apply(TallyEvent {
            target: tile(2),
            state: TallyState::preview(),
        });
        mirror.apply(TallyEvent {
            target: tile(1),
            state: TallyState::program(),
        });
        // Latest wins for the same target.
        mirror.apply(TallyEvent {
            target: tile(2),
            state: TallyState::program(),
        });

        assert_eq!(mirror.len(), 2);
        let got = mirror.get(&tile(2)).unwrap();
        assert_eq!(got.state.color, TallyColor::Red);

        let listed = mirror.list();
        let colors: Vec<TallyColor> = listed.iter().map(|e| e.state.color).collect();
        // key-sorted: tile:1 before tile:2.
        assert_eq!(colors, vec![TallyColor::Red, TallyColor::Red]);
    }

    #[test]
    fn override_registry_set_get_clear() {
        let reg = OverrideRegistry::new();
        assert!(reg.is_empty());
        reg.set(&tile(0), TallyColor::Amber);
        assert_eq!(reg.get(&tile(0)), Some(TallyColor::Amber));
        assert_eq!(reg.len(), 1);
        // Overwrite.
        reg.set(&tile(0), TallyColor::Red);
        assert_eq!(reg.get(&tile(0)), Some(TallyColor::Red));
        // Clear.
        assert!(reg.clear(&tile(0)));
        assert!(!reg.clear(&tile(0)));
        assert!(reg.is_empty());
    }

    // `TallyProfile` is `#[non_exhaustive]`; build it via serde.
    fn profile(id: &str) -> TallyProfile {
        serde_json::from_value(serde_json::json!({ "id": id })).expect("tally profile deserialises")
    }

    #[test]
    fn profile_put_inserts_then_bumps_and_delete_removes() {
        let store = InMemoryProfileStore::new();
        let v1 = store.put(profile("p1")).unwrap();
        assert_eq!(v1.version, Version::INITIAL);
        let v2 = store.put(profile("p1")).unwrap();
        assert_eq!(v2.version, Version::INITIAL.next());

        store.put(profile("p0")).unwrap();
        let ids: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|v| v.profile.id)
            .collect();
        assert_eq!(ids, vec!["p0", "p1"]);

        store.delete("p1").unwrap();
        let err = store.delete("p1").unwrap_err();
        assert!(matches!(
            err,
            ControlError::NotFound {
                kind: TALLY_PROFILE_KIND,
                ..
            }
        ));
    }

    #[test]
    fn profile_get_unknown_is_not_found() {
        let store = InMemoryProfileStore::new();
        let err = store.get("missing").unwrap_err();
        assert!(matches!(err, ControlError::NotFound { .. }));
    }
}
