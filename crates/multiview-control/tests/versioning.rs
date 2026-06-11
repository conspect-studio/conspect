//! Config-versioning tests: immutable revisions of a config/layout document with
//! diff and rollback. Rollback restores the prior state as a NEW revision (the
//! history is append-only — no revision is ever rewritten).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::{
    diff_documents, ConfigVersionStore, DocumentDiff, InMemoryConfigVersionStore, RevisionId,
};
use serde_json::json;

#[test]
fn revisions_are_immutable_and_monotonic() {
    let store = InMemoryConfigVersionStore::new();
    let r1 = store
        .commit("layout:wall", json!({ "cells": 4 }), "admin-key", "initial")
        .unwrap();
    let r2 = store
        .commit(
            "layout:wall",
            json!({ "cells": 9 }),
            "operator-key",
            "expand",
        )
        .unwrap();
    assert_eq!(r1.revision, RevisionId::new(1));
    assert_eq!(r2.revision, RevisionId::new(2));

    // The first revision still reads back unchanged (immutability).
    let fetched = store.get("layout:wall", RevisionId::new(1)).unwrap();
    assert_eq!(fetched.document, json!({ "cells": 4 }));
    assert_eq!(fetched.author, "admin-key");
    assert_eq!(fetched.message, "initial");

    // History lists newest-first.
    let history = store.history("layout:wall").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].revision, RevisionId::new(2));
    assert_eq!(history[1].revision, RevisionId::new(1));

    // The current (head) revision is the latest commit.
    let head = store.head("layout:wall").unwrap();
    assert_eq!(head.revision, RevisionId::new(2));
    assert_eq!(head.document, json!({ "cells": 9 }));
}

#[test]
fn rollback_restores_prior_state_as_a_new_revision() {
    let store = InMemoryConfigVersionStore::new();
    store
        .commit("layout:wall", json!({ "cells": 4 }), "admin-key", "v1")
        .unwrap();
    store
        .commit("layout:wall", json!({ "cells": 9 }), "admin-key", "v2")
        .unwrap();

    // Roll back to revision 1.
    let rolled = store
        .rollback("layout:wall", RevisionId::new(1), "operator-key")
        .unwrap();

    // The rollback is an APPEND: a new revision (3) whose document equals r1's.
    assert_eq!(rolled.revision, RevisionId::new(3));
    assert_eq!(rolled.document, json!({ "cells": 4 }));
    assert_eq!(rolled.author, "operator-key");

    // The head now reflects the rolled-back document.
    let head = store.head("layout:wall").unwrap();
    assert_eq!(head.document, json!({ "cells": 4 }));

    // The intermediate revision 2 is preserved (append-only history).
    let r2 = store.get("layout:wall", RevisionId::new(2)).unwrap();
    assert_eq!(r2.document, json!({ "cells": 9 }));
    assert_eq!(store.history("layout:wall").unwrap().len(), 3);
}

#[test]
fn rollback_to_unknown_revision_is_an_error() {
    let store = InMemoryConfigVersionStore::new();
    store
        .commit("layout:wall", json!({}), "admin-key", "v1")
        .unwrap();
    assert!(store
        .rollback("layout:wall", RevisionId::new(99), "admin-key")
        .is_err());
    assert!(store
        .rollback("layout:missing", RevisionId::new(1), "admin-key")
        .is_err());
}

#[test]
fn diff_reports_added_removed_and_changed_keys() {
    let a = json!({ "keep": 1, "drop": 2, "change": "old" });
    let b = json!({ "keep": 1, "change": "new", "add": 3 });
    let d: DocumentDiff = diff_documents(&a, &b);

    assert_eq!(d.added, vec!["add".to_owned()]);
    assert_eq!(d.removed, vec!["drop".to_owned()]);
    assert_eq!(d.changed, vec!["change".to_owned()]);
    assert!(!d.is_empty());

    // Identical documents diff empty.
    let same = diff_documents(&a, &a);
    assert!(same.is_empty());
    assert!(same.added.is_empty() && same.removed.is_empty() && same.changed.is_empty());
}

#[test]
fn diff_between_two_revisions_via_store() {
    let store = InMemoryConfigVersionStore::new();
    store
        .commit("cfg", json!({ "a": 1 }), "admin-key", "v1")
        .unwrap();
    store
        .commit("cfg", json!({ "a": 2, "b": 3 }), "admin-key", "v2")
        .unwrap();
    let d = store
        .diff("cfg", RevisionId::new(1), RevisionId::new(2))
        .unwrap();
    assert_eq!(d.added, vec!["b".to_owned()]);
    assert_eq!(d.changed, vec!["a".to_owned()]);
    assert!(d.removed.is_empty());
}
