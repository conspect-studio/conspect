//! Runtime smoke test for the `sqlite`-feature repository (off by default; NOT
//! part of the CI-green default build). Validates the real CRUD + versioning
//! logic against an in-memory `SQLite` database.
#![cfg(feature = "sqlite")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::sqlite::{SqliteAlarmStore, SqliteRepository};
use multiview_control::{
    AlarmFilter, AlarmRepository, ControlError, LayoutInput, Repository, Version,
};
use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;
use serde_json::json;

fn input(name: &str) -> LayoutInput {
    LayoutInput {
        name: name.to_owned(),
        body: json!({ "cells": [] }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_crud_round_trips_with_versioning() {
    let repo = SqliteRepository::connect("sqlite::memory:")
        .await
        .expect("connect to in-memory sqlite");

    // Create -> version 1.
    let created = repo.create_layout("main", input("Main")).expect("create");
    assert_eq!(created.version, Version::INITIAL);
    assert_eq!(created.layout.name, "Main");

    // Duplicate create is rejected.
    assert!(matches!(
        repo.create_layout("main", input("Dup")),
        Err(ControlError::Validation(_))
    ));

    // Get returns it.
    let fetched = repo.get_layout("main").expect("get");
    assert_eq!(fetched.layout.name, "Main");

    // Update bumps the version.
    let updated = repo
        .update_layout("main", input("Renamed"))
        .expect("update");
    assert_eq!(updated.version, Version::new(2));
    assert_eq!(updated.layout.name, "Renamed");

    // List is id-sorted.
    repo.create_layout("aaa", input("A")).expect("create aaa");
    let list = repo.list_layouts().expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].layout.id, "aaa");
    assert_eq!(list[1].layout.id, "main");

    // Delete removes it; a second delete is NotFound.
    repo.delete_layout("main").expect("delete");
    assert!(matches!(
        repo.get_layout("main"),
        Err(ControlError::NotFound { .. })
    ));
    assert!(matches!(
        repo.delete_layout("main"),
        Err(ControlError::NotFound { .. })
    ));
}

fn alarm(id: &str, severity: PerceivedSeverity) -> AlarmRecord {
    AlarmRecord::new(
        AlarmId::new(id),
        AlarmKind::Black,
        severity,
        AlarmScope::Tile { index: 0 },
        MediaTime::from_nanos(7),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_alarm_store_upserts_acks_and_filters_with_versioning() {
    let store = SqliteAlarmStore::connect("sqlite::memory:")
        .await
        .expect("connect to in-memory sqlite");

    // Upsert -> version 1; identical upsert does not churn the version.
    let v1 = store.upsert(alarm("a", PerceivedSeverity::Major)).unwrap();
    assert_eq!(v1.version, Version::INITIAL);
    let v1b = store.upsert(alarm("a", PerceivedSeverity::Major)).unwrap();
    assert_eq!(v1b.version, Version::INITIAL, "identical upsert is a no-op");

    // A changed record bumps the version.
    let v2 = store
        .upsert(alarm("a", PerceivedSeverity::Critical))
        .unwrap();
    assert_eq!(v2.version, Version::new(2));

    // Acknowledge sets the ack state and bumps the version again.
    let acked = store
        .acknowledge(&AlarmId::new("a"), "alice", MediaTime::from_nanos(99))
        .unwrap();
    assert_eq!(acked.version, Version::new(3));
    assert!(acked.record.ack.is_acked());

    // Acknowledging an unknown alarm is NotFound.
    assert!(matches!(
        store.acknowledge(&AlarmId::new("missing"), "bob", MediaTime::ZERO),
        Err(ControlError::NotFound { .. })
    ));

    // Filtering by severity is applied by the store.
    store
        .upsert(alarm("b", PerceivedSeverity::Warning))
        .unwrap();
    let majors = store
        .list(&AlarmFilter {
            min_severity: Some(PerceivedSeverity::Major),
            ..AlarmFilter::default()
        })
        .unwrap();
    let ids: Vec<&str> = majors.iter().map(|v| v.record.id.as_str()).collect();
    assert_eq!(ids, vec!["a"], "only the Critical alarm 'a' is >= Major");
}
