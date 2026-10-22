//! Finer RBAC tests: the role hierarchy gains a strictly read-only role, and a
//! principal can be scoped to a set of output ids (output-scoped operator). An
//! output-scoped principal is denied any output id outside its allowlist even
//! when its role would otherwise permit the action (BOLA, per-output).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::{authorize_output, Action, ControlError, Principal, Role};

fn output_scoped(outputs: &[&str]) -> Principal {
    Principal {
        key_id: "op-out".to_owned(),
        role: Role::Operator,
        scoped_object_ids: None,
        scoped_output_ids: Some(outputs.iter().map(|s| (*s).to_owned()).collect()),
        scoped_discovery_domains: None,
    }
}

#[test]
fn read_only_role_permits_only_reads() {
    assert!(Role::ReadOnly.can(Action::Read));
    assert!(!Role::ReadOnly.can(Action::Write));
    assert!(!Role::ReadOnly.can(Action::Administer));
    // ReadOnly is the lowest privilege, below Viewer.
    assert!(Role::ReadOnly < Role::Viewer);
    assert!(Role::ReadOnly < Role::Operator);
    assert!(Role::ReadOnly < Role::Admin);
}

#[test]
fn role_hierarchy_is_total_and_ordered() {
    // ReadOnly < Viewer < Operator < Admin.
    let mut roles = [Role::Admin, Role::ReadOnly, Role::Operator, Role::Viewer];
    roles.sort();
    assert_eq!(
        roles,
        [Role::ReadOnly, Role::Viewer, Role::Operator, Role::Admin]
    );
}

#[test]
fn output_scoped_principal_denied_outputs_outside_its_allowlist() {
    let p = output_scoped(&["out-a", "out-b"]);

    // Allowed outputs pass.
    assert!(authorize_output(&p, "out-a").is_ok());
    assert!(authorize_output(&p, "out-b").is_ok());

    // A cross-output id is denied even though the role is Operator (BOLA).
    let err = authorize_output(&p, "out-c").unwrap_err();
    assert!(matches!(err, ControlError::Forbidden(_)), "{err:?}");
}

#[test]
fn output_unscoped_principal_may_access_any_output() {
    let admin = Principal {
        key_id: "admin".to_owned(),
        role: Role::Admin,
        scoped_object_ids: None,
        scoped_output_ids: None,
        scoped_discovery_domains: None,
    };
    assert!(authorize_output(&admin, "any-output").is_ok());
    assert!(authorize_output(&admin, "another").is_ok());
}

#[test]
fn output_and_object_scopes_are_independent() {
    // A principal may be scoped on objects but unrestricted on outputs (or vice
    // versa); each guard is checked independently.
    let p = Principal {
        key_id: "mixed".to_owned(),
        role: Role::Operator,
        scoped_object_ids: Some(vec!["layout-1".to_owned()]),
        scoped_output_ids: Some(vec!["out-1".to_owned()]),
        scoped_discovery_domains: None,
    };
    // Object guard.
    assert!(multiview_control::authorize_object(&p, "layout-1").is_ok());
    assert!(multiview_control::authorize_object(&p, "layout-2").is_err());
    // Output guard (independent).
    assert!(authorize_output(&p, "out-1").is_ok());
    assert!(authorize_output(&p, "out-2").is_err());
}
