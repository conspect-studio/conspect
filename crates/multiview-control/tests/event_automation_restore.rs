//! F1/F2 regressions for the ADR-0183 safety model's owned-override restore.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::match_same_arms
)]
use multiview_control::auth::{Principal, Role};
use multiview_control::event_automation::*;
use std::cell::RefCell;
use std::collections::BTreeSet;

struct Adapter {
    principal: Principal,
    missing: RefCell<BTreeSet<String>>,
}
impl Adapter {
    fn new() -> Self {
        Self {
            principal: Principal {
                key_id: "authenticated-test".into(),
                role: Role::Operator,
                scoped_object_ids: None,
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
            missing: RefCell::new(BTreeSet::new()),
        }
    }
    fn delete(&self, id: &str) {
        self.missing.borrow_mut().insert(id.to_owned());
    }
}
impl Authorization for Adapter {
    fn principal(&self) -> &Principal {
        &self.principal
    }
    fn resource_exists(&self, r: &Resource) -> bool {
        match r {
            Resource::Object(id) => !self.missing.borrow().contains(id),
            Resource::Destination(Scope::Tile(id)) => !self.missing.borrow().contains(id),
            Resource::Destination(_) => true,
        }
    }
    fn vt_ready(&self, _: &str) -> bool {
        true
    }
}
fn replacement() -> Rule {
    Rule {
        id: "rule".into(),
        source: "source".into(),
        trigger: BoundaryKind::Start,
        action: Action::Replace {
            scope: Scope::Tile("cell".into()),
            video: Replacement::Bars,
            audio: AudioPolicy::Original,
            expected_generation: 7,
            max_hold_ns: 50,
        },
        allow_unpresented: false,
        max_lateness_ns: 10,
    }
}
fn event() -> Boundary {
    Boundary {
        event: "event".into(),
        source: "source".into(),
        epoch: 1,
        boundary: BoundaryKind::Start,
        reliable: true,
        current: true,
        presented: true,
        effective_ns: 100,
        position_ns: 100,
        monotonic_ns: 100,
    }
}
fn applied(m: &mut SafetyModel, a: &Adapter) -> u64 {
    m.set_mode(Mode::Armed);
    let Decision::Reserved { id, .. } = m.reserve(&replacement(), &event(), a).unwrap() else {
        panic!("expected reservation")
    };
    m.acknowledge(id, Outcome::Applied { generation: 8 })
        .unwrap();
    id
}

/// F1: deleting the trigger's rule or source must not strand the replacement.
#[test]
fn owned_restore_survives_rule_and_source_deletion() {
    for deleted in ["rule", "source"] {
        let a = Adapter::new();
        let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
        let id = applied(&mut m, &a);
        a.delete(deleted);
        let plan = m.restore(id, 8, &a).unwrap();
        assert!(matches!(
            plan.action,
            Action::Restore {
                expected_generation: 8,
                ..
            }
        ));
        m.finish_restore(id, true).unwrap();
    }
}

/// F1: restore still requires CURRENT destination permission/existence and
/// the manual-generation compare; a denial does not consume the lease.
#[test]
fn owned_restore_checks_destination_permission_and_generation() {
    let mut a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    let id = applied(&mut m, &a);
    a.principal.scoped_object_ids = Some(vec!["rule".into(), "source".into()]);
    assert_eq!(m.restore_at(id, 8, 200, &a), Err(Rejection::Unauthorized));
    a.principal.scoped_object_ids = None;
    a.principal.role = Role::Viewer;
    assert_eq!(m.restore_at(id, 8, 200, &a), Err(Rejection::Unauthorized));
    a.principal.role = Role::Operator;
    a.delete("cell");
    assert_eq!(
        m.restore_at(id, 8, 200, &a),
        Err(Rejection::MissingResource)
    );
    a.missing.borrow_mut().clear();
    assert_eq!(
        m.restore_at(id, 9, 200, &a),
        Err(Rejection::GenerationConflict)
    );
    assert_eq!(
        m.restore_at(id, 8, 200, &a),
        Err(Rejection::GenerationConflict)
    );
}

/// F2: a dropped restore dispatch becomes reconcilable after a bounded
/// in-flight timeout instead of suppressing the watchdog forever.
#[test]
fn dropped_restore_dispatch_is_reconciled_and_retried() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    let id = applied(&mut m, &a);
    assert_eq!(m.expired_leases(150), vec![id]);
    m.restore_at(id, 8, 150, &a).unwrap();
    assert!(m.expired_leases(10_000).is_empty());
    assert!(m
        .stalled_restores(150 + RESTORE_ACK_TIMEOUT_NS - 1)
        .is_empty());
    assert_eq!(m.stalled_restores(150 + RESTORE_ACK_TIMEOUT_NS), vec![id]);
    // Destination still at our generation: the restore never landed.
    assert_eq!(
        m.reconcile_restore(id, RestoreObservation::Unchanged),
        Ok(Reconciled::Retry)
    );
    assert_eq!(m.expired_leases(10_000), vec![id], "watchdog is live again");
    m.restore_at(id, 8, 10_000, &a).unwrap();
    m.finish_restore(id, true).unwrap();
    assert!(m.expired_leases(u64::MAX).is_empty());
}

/// F2: an ack lost AFTER the restore applied must finish, never re-restore.
#[test]
fn lost_ack_after_application_finishes_without_retry() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    let id = applied(&mut m, &a);
    m.restore_at(id, 8, 150, &a).unwrap();
    let late = 150 + RESTORE_ACK_TIMEOUT_NS;
    assert_eq!(m.stalled_restores(late), vec![id]);
    assert_eq!(
        m.reconcile_restore(id, RestoreObservation::Restored),
        Ok(Reconciled::Finished)
    );
    assert!(m.stalled_restores(u64::MAX).is_empty());
    assert_eq!(m.restore_at(id, 8, late, &a), Err(Rejection::NotApplied));
}

/// F2: a manual change observed during reconciliation revokes; retries are
/// bounded so an unknown outcome never becomes a blind retry loop.
#[test]
fn manual_change_revokes_and_retries_are_bounded() {
    let a = Adapter::new();
    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    let id = applied(&mut m, &a);
    m.restore_at(id, 8, 150, &a).unwrap();
    assert_eq!(
        m.reconcile_restore(id, RestoreObservation::Changed),
        Ok(Reconciled::Revoked)
    );
    assert_eq!(
        m.restore_at(id, 8, 150, &a),
        Err(Rejection::GenerationConflict)
    );
    assert!(m.expired_leases(u64::MAX).is_empty());

    let mut m = SafetyModel::new("source".into(), 8, 2, 1, 0).unwrap();
    let id = applied(&mut m, &a);
    let mut outcome = Reconciled::Retry;
    for attempt in 0..MAX_RESTORE_ATTEMPTS {
        m.restore_at(id, 8, 150, &a).unwrap();
        outcome = m
            .reconcile_restore(id, RestoreObservation::Unchanged)
            .unwrap();
        if attempt + 1 < MAX_RESTORE_ATTEMPTS {
            assert_eq!(outcome, Reconciled::Retry);
        }
    }
    assert_eq!(outcome, Reconciled::Revoked);
    assert_eq!(
        m.restore_at(id, 8, 150, &a),
        Err(Rejection::GenerationConflict)
    );
    // Reconciling a lease with no in-flight restore is rejected.
    assert_eq!(
        m.reconcile_restore(id, RestoreObservation::Unchanged),
        Err(Rejection::NotApplied)
    );
}
