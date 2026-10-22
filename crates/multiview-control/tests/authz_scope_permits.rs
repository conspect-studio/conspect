#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! The shared fail-closed authorization predicate `scope_permits` and its
//! `authorize_scope` 403 twin (ADR-W026): the single rule the realtime filter
//! and REST both route through. Pins the per-axis truth table, the fail-closed
//! `(scoped, unlabelled) => deny`, and the `program:` namespace separation that
//! keeps a timing grant from punning the plain output-id namespace.

use multiview_control::auth::{
    authorize_object, authorize_output, authorize_scope, scope_permits, Principal, Role,
};
use multiview_events::AuthzScope;

/// A scoped principal with the three axes set explicitly.
fn principal(
    objects: Option<Vec<&str>>,
    outputs: Option<Vec<&str>>,
    domains: Option<Vec<&str>>,
) -> Principal {
    let own = |v: Option<Vec<&str>>| v.map(|xs| xs.into_iter().map(str::to_owned).collect());
    Principal {
        key_id: "k".to_owned(),
        role: Role::Operator,
        scoped_object_ids: own(objects),
        scoped_output_ids: own(outputs),
        scoped_discovery_domains: own(domains),
    }
}

#[test]
fn public_is_always_permitted() {
    let p = principal(Some(vec![]), Some(vec![]), Some(vec![]));
    assert!(scope_permits(&p.scopes(), AuthzScope::Public));
}

#[test]
fn object_axis_none_sees_all_some_is_allowlist() {
    let unscoped = principal(None, None, None);
    assert!(scope_permits(
        &unscoped.scopes(),
        AuthzScope::Object("cam-3")
    ));

    let scoped = principal(Some(vec!["cam-3"]), None, None);
    assert!(scope_permits(&scoped.scopes(), AuthzScope::Object("cam-3")));
    assert!(!scope_permits(
        &scoped.scopes(),
        AuthzScope::Object("cam-9")
    ));

    let closed = principal(Some(vec![]), None, None);
    assert!(!scope_permits(
        &closed.scopes(),
        AuthzScope::Object("cam-3")
    ));
}

#[test]
fn discovery_domain_fail_closed_on_unlabelled() {
    let scoped = principal(None, None, Some(vec!["site-a"]));
    assert!(scope_permits(
        &scoped.scopes(),
        AuthzScope::DiscoveryDomain(Some("site-a"))
    ));
    assert!(!scope_permits(
        &scoped.scopes(),
        AuthzScope::DiscoveryDomain(Some("site-b"))
    ));
    // The load-bearing fail-closed rule: a scoped principal never sees an
    // unlabelled row.
    assert!(!scope_permits(
        &scoped.scopes(),
        AuthzScope::DiscoveryDomain(None)
    ));

    // Unscoped principals see everything, including unlabelled rows (compat).
    let unscoped = principal(None, None, None);
    assert!(scope_permits(
        &unscoped.scopes(),
        AuthzScope::DiscoveryDomain(None)
    ));
    assert!(scope_permits(
        &unscoped.scopes(),
        AuthzScope::DiscoveryDomain(Some("site-x"))
    ));
}

#[test]
fn program_grant_is_namespaced_and_does_not_pun_output() {
    // A key granted `program:main` (only) receives timing for `main` but has NO
    // authority over an output literally named `main`.
    let timing_only = principal(None, Some(vec!["program:main"]), None);
    assert!(scope_permits(
        &timing_only.scopes(),
        AuthzScope::Program("main")
    ));
    assert!(!scope_permits(
        &timing_only.scopes(),
        AuthzScope::Output("main")
    ));

    // Conversely, a key granted the plain output `main` gets that output but NOT
    // the program timing epoch (must add `program:main` explicitly).
    let output_only = principal(None, Some(vec!["main"]), None);
    assert!(scope_permits(
        &output_only.scopes(),
        AuthzScope::Output("main")
    ));
    assert!(!scope_permits(
        &output_only.scopes(),
        AuthzScope::Program("main")
    ));

    // Output-unscoped sees both.
    let unscoped = principal(None, None, None);
    assert!(scope_permits(
        &unscoped.scopes(),
        AuthzScope::Program("main")
    ));
    assert!(scope_permits(
        &unscoped.scopes(),
        AuthzScope::Output("main")
    ));
}

#[test]
fn object_and_output_is_a_conjunction() {
    let both = principal(Some(vec!["preset-a"]), Some(vec!["out-1"]), None);
    let salvo = AuthzScope::ObjectAndOutput {
        object: "preset-a",
        output: "out-1",
    };
    assert!(scope_permits(&both.scopes(), salvo));

    // Either mismatch denies; a single-axis check would under-gate this event
    // against its REST twin (`authorize_object` AND `authorize_output`).
    let wrong_object = principal(Some(vec!["preset-b"]), Some(vec!["out-1"]), None);
    assert!(!scope_permits(&wrong_object.scopes(), salvo));

    let wrong_output = principal(Some(vec!["preset-a"]), Some(vec!["out-2"]), None);
    assert!(!scope_permits(&wrong_output.scopes(), salvo));

    // Unset on one axis is unrestricted only on THAT axis; the other still gates.
    let object_unscoped = principal(None, Some(vec!["out-1"]), None);
    assert!(scope_permits(&object_unscoped.scopes(), salvo));
    let output_unscoped = principal(Some(vec!["preset-a"]), None, None);
    assert!(scope_permits(&output_unscoped.scopes(), salvo));
}

#[test]
fn authorize_scope_is_the_403_twin_of_the_predicate() {
    let scoped = principal(None, None, Some(vec!["site-a"]));
    assert!(authorize_scope(&scoped, AuthzScope::DiscoveryDomain(Some("site-a"))).is_ok());
    assert!(authorize_scope(&scoped, AuthzScope::DiscoveryDomain(None)).is_err());
}

#[test]
fn authorize_object_and_output_wrappers_preserve_semantics() {
    let obj_scoped = principal(Some(vec!["cam-3"]), None, None);
    assert!(authorize_object(&obj_scoped, "cam-3").is_ok());
    assert!(authorize_object(&obj_scoped, "cam-9").is_err());

    // A `program:*` entry is inert for plain output authorization.
    let timing_only = principal(None, Some(vec!["program:main"]), None);
    assert!(authorize_output(&timing_only, "main").is_err());

    let out_scoped = principal(None, Some(vec!["out-1"]), None);
    assert!(authorize_output(&out_scoped, "out-1").is_ok());
    assert!(authorize_output(&out_scoped, "out-2").is_err());
}
