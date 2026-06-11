//! Config **versioning** (M6; broadcast brief §8 "config versioning",
//! ADR-M006 config-as-code rollback).
//!
//! A config/layout document is tracked as an **append-only** sequence of
//! immutable [`ConfigRevision`]s under a stable `target` key (e.g.
//! `layout:wall`). Committing a new document appends a revision with a
//! monotonically increasing [`RevisionId`]; the head is the latest commit.
//! [`ConfigVersionStore::rollback`] restores a prior revision's document **as a
//! new revision** — the history is never rewritten, so an operator can always
//! audit and re-roll. [`diff_documents`] reports the added/removed/changed
//! top-level keys between two documents (the shape the UI surfaces).
//!
//! The store holds control-plane state only and is never on the engine's data
//! plane, so it cannot back-pressure the engine (invariant #10).
use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{ControlError, ControlResult};

/// The resource collection name used in problem documents and routes.
pub const CONFIG_REVISION_KIND: &str = "config-revision";

/// A monotonic, per-target revision number (starts at 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(transparent)]
pub struct RevisionId(u64);

impl RevisionId {
    /// The first revision of any target.
    pub const FIRST: Self = Self(1);

    /// Construct a revision id from a raw counter.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next revision id (saturating at [`u64::MAX`]).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// One immutable revision of a target's config/layout document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConfigRevision {
    /// The stable target key this revision belongs to (e.g. `layout:wall`).
    pub target: String,
    /// This revision's monotonic id.
    pub revision: RevisionId,
    /// The committed document (opaque canonical JSON).
    pub document: serde_json::Value,
    /// The principal that committed this revision (its key id / subject).
    pub author: String,
    /// A short human commit message (e.g. `"expand to 9 cells"`).
    pub message: String,
}

/// The added / removed / changed top-level keys between two documents.
///
/// A pragmatic, UI-facing structural diff over JSON objects: it reports which
/// top-level keys were added, removed, or whose value changed. Non-object
/// documents (or a type change at the root) are reported as a single synthetic
/// `<root>` change so the diff is always total.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DocumentDiff {
    /// Keys present in the new document but not the old (sorted).
    pub added: Vec<String>,
    /// Keys present in the old document but not the new (sorted).
    pub removed: Vec<String>,
    /// Keys present in both whose value differs (sorted).
    pub changed: Vec<String>,
}

impl DocumentDiff {
    /// Whether the two documents are structurally identical (no added, removed,
    /// or changed keys).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// The synthetic key reported when the documents differ at a non-object root.
const ROOT_KEY: &str = "<root>";

/// Compute the top-level structural [`DocumentDiff`] from `old` to `new`.
#[must_use]
pub fn diff_documents(old: &serde_json::Value, new: &serde_json::Value) -> DocumentDiff {
    match (old.as_object(), new.as_object()) {
        (Some(old_map), Some(new_map)) => {
            let mut diff = DocumentDiff::default();
            for (k, new_v) in new_map {
                match old_map.get(k) {
                    None => diff.added.push(k.clone()),
                    Some(old_v) if old_v != new_v => diff.changed.push(k.clone()),
                    Some(_) => {}
                }
            }
            for k in old_map.keys() {
                if !new_map.contains_key(k) {
                    diff.removed.push(k.clone());
                }
            }
            diff.added.sort();
            diff.removed.sort();
            diff.changed.sort();
            diff
        }
        _ => {
            // Either side is not a JSON object (or the root type changed):
            // report a single synthetic root change iff the values differ.
            if old == new {
                DocumentDiff::default()
            } else {
                DocumentDiff {
                    added: Vec::new(),
                    removed: Vec::new(),
                    changed: vec![ROOT_KEY.to_owned()],
                }
            }
        }
    }
}

/// Append-only, immutable-revision store for config/layout documents with diff
/// and rollback.
pub trait ConfigVersionStore: Send + Sync + 'static {
    /// Commit `document` as the next revision of `target`, returning the new
    /// immutable [`ConfigRevision`].
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] on a backing-store fault.
    fn commit(
        &self,
        target: &str,
        document: serde_json::Value,
        author: &str,
        message: &str,
    ) -> ControlResult<ConfigRevision>;

    /// Fetch a specific revision of `target`.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if the target or revision does not exist.
    fn get(&self, target: &str, revision: RevisionId) -> ControlResult<ConfigRevision>;

    /// The current head (latest) revision of `target`.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if the target has no revisions.
    fn head(&self, target: &str) -> ControlResult<ConfigRevision>;

    /// All revisions of `target`, **newest-first**.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if the target has no revisions.
    fn history(&self, target: &str) -> ControlResult<Vec<ConfigRevision>>;

    /// Roll `target` back to a prior revision by **appending** a new revision
    /// whose document equals that revision's. The history is never rewritten.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if the target or revision does not exist.
    fn rollback(&self, target: &str, to: RevisionId, author: &str)
        -> ControlResult<ConfigRevision>;

    /// The top-level [`DocumentDiff`] between two revisions of `target`
    /// (`from` → `to`).
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if either revision does not exist.
    fn diff(&self, target: &str, from: RevisionId, to: RevisionId) -> ControlResult<DocumentDiff>;
}

/// An in-memory [`ConfigVersionStore`] backed by a `Mutex<HashMap>` of
/// per-target revision vectors.
#[derive(Debug, Default)]
pub struct InMemoryConfigVersionStore {
    targets: Mutex<HashMap<String, Vec<ConfigRevision>>>,
}

impl InMemoryConfigVersionStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<ConfigRevision>>> {
        match self.targets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Locate a revision within a target's history.
    fn find<'a>(
        revisions: &'a [ConfigRevision],
        target: &str,
        revision: RevisionId,
    ) -> ControlResult<&'a ConfigRevision> {
        revisions
            .iter()
            .find(|r| r.revision == revision)
            .ok_or_else(|| ControlError::NotFound {
                kind: CONFIG_REVISION_KIND,
                id: format!("{target}@{}", revision.get()),
            })
    }
}

impl ConfigVersionStore for InMemoryConfigVersionStore {
    fn commit(
        &self,
        target: &str,
        document: serde_json::Value,
        author: &str,
        message: &str,
    ) -> ControlResult<ConfigRevision> {
        let mut guard = self.lock();
        let revisions = guard.entry(target.to_owned()).or_default();
        let revision = revisions
            .last()
            .map_or(RevisionId::FIRST, |r| r.revision.next());
        let entry = ConfigRevision {
            target: target.to_owned(),
            revision,
            document,
            author: author.to_owned(),
            message: message.to_owned(),
        };
        revisions.push(entry.clone());
        Ok(entry)
    }

    fn get(&self, target: &str, revision: RevisionId) -> ControlResult<ConfigRevision> {
        let guard = self.lock();
        let revisions = guard.get(target).ok_or_else(|| ControlError::NotFound {
            kind: CONFIG_REVISION_KIND,
            id: target.to_owned(),
        })?;
        Self::find(revisions, target, revision).cloned()
    }

    fn head(&self, target: &str) -> ControlResult<ConfigRevision> {
        let guard = self.lock();
        guard
            .get(target)
            .and_then(|revisions| revisions.last())
            .cloned()
            .ok_or_else(|| ControlError::NotFound {
                kind: CONFIG_REVISION_KIND,
                id: target.to_owned(),
            })
    }

    fn history(&self, target: &str) -> ControlResult<Vec<ConfigRevision>> {
        let guard = self.lock();
        let revisions =
            guard
                .get(target)
                .filter(|r| !r.is_empty())
                .ok_or_else(|| ControlError::NotFound {
                    kind: CONFIG_REVISION_KIND,
                    id: target.to_owned(),
                })?;
        // Newest-first.
        Ok(revisions.iter().rev().cloned().collect())
    }

    fn rollback(
        &self,
        target: &str,
        to: RevisionId,
        author: &str,
    ) -> ControlResult<ConfigRevision> {
        let mut guard = self.lock();
        let revisions = guard
            .get_mut(target)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| ControlError::NotFound {
                kind: CONFIG_REVISION_KIND,
                id: target.to_owned(),
            })?;
        // The document to restore (cloned out before we mutate the vector).
        let restored = Self::find(revisions, target, to)?.document.clone();
        let next = revisions
            .last()
            .map_or(RevisionId::FIRST, |r| r.revision.next());
        let entry = ConfigRevision {
            target: target.to_owned(),
            revision: next,
            document: restored,
            author: author.to_owned(),
            message: format!("rollback to revision {}", to.get()),
        };
        revisions.push(entry.clone());
        Ok(entry)
    }

    fn diff(&self, target: &str, from: RevisionId, to: RevisionId) -> ControlResult<DocumentDiff> {
        let guard = self.lock();
        let revisions = guard.get(target).ok_or_else(|| ControlError::NotFound {
            kind: CONFIG_REVISION_KIND,
            id: target.to_owned(),
        })?;
        let from_doc = &Self::find(revisions, target, from)?.document;
        let to_doc = &Self::find(revisions, target, to)?.document;
        Ok(diff_documents(from_doc, to_doc))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{diff_documents, ConfigVersionStore, InMemoryConfigVersionStore, RevisionId};
    use serde_json::json;

    #[test]
    fn non_object_root_diff_reports_root_change() {
        let d = diff_documents(&json!(1), &json!(2));
        assert_eq!(d.changed, vec!["<root>".to_owned()]);
        assert!(diff_documents(&json!(1), &json!(1)).is_empty());
    }

    #[test]
    fn head_tracks_latest_commit() {
        let store = InMemoryConfigVersionStore::new();
        store.commit("t", json!({}), "a", "1").unwrap();
        let r2 = store.commit("t", json!({ "x": 1 }), "a", "2").unwrap();
        assert_eq!(store.head("t").unwrap().revision, r2.revision);
        assert_eq!(store.head("t").unwrap().revision, RevisionId::new(2));
    }

    #[test]
    fn unknown_target_is_not_found() {
        let store = InMemoryConfigVersionStore::new();
        assert!(store.head("missing").is_err());
        assert!(store.history("missing").is_err());
        assert!(store.get("missing", RevisionId::FIRST).is_err());
    }
}
