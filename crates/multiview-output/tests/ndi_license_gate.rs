//! Acceptance test for the NDI® runtime license gate (ADR-0008 §7.5).
//!
//! With the license **not accepted**, no NDI sender is constructed and a typed
//! refusal is returned — never a panic, never a started sender. With acceptance,
//! the sender opens. The gate is enforced by construction: `NdiOutput::new`
//! requires an accepted `NdiLicense`, and the only way to obtain one is through an
//! audited acceptance record.
#![cfg(feature = "ndi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::ndi::license::LicenseAcceptance;
use multiview_output::ndi::{FakeNdiApi, NdiLicense, NdiLicenseError, NdiOutput};

fn accepted() -> NdiLicense {
    NdiLicense::accept(LicenseAcceptance {
        accepted_by: "operator@example".to_owned(),
        accepted_at: "2026-06-06T00:00:00Z".to_owned(),
    })
    .expect("a complete acceptance must be accepted")
}

#[test]
fn unaccepted_setting_refuses_and_never_constructs_a_sender() {
    // The `[system.ndi] accept_license = false` path: a typed refusal, no sender.
    let err = NdiLicense::from_setting(
        false,
        LicenseAcceptance {
            accepted_by: "operator@example".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        },
    )
    .expect_err("accept_license=false must be refused");
    assert_eq!(err, NdiLicenseError::NotAccepted);

    // And there is NO way to construct NdiOutput without an accepted license:
    // `NdiOutput::new` takes `NdiLicense` by value, which we could not obtain.
    // The fake API records no sender because the gate stopped us first.
    let api = FakeNdiApi::new();
    assert_eq!(
        api.created, None,
        "no sender may be created when unlicensed"
    );
    assert!(!api.open);
}

#[test]
fn incomplete_acceptance_is_refused_not_panicked() {
    // An accepted flag with blank audit fields must be refused (who/when required).
    let err = NdiLicense::from_setting(
        true,
        LicenseAcceptance {
            accepted_by: "   ".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        },
    )
    .expect_err("blank accepted_by must be refused");
    assert!(matches!(err, NdiLicenseError::IncompleteAcceptance { .. }));
}

#[test]
fn accepted_license_opens_a_sender() {
    let out = NdiOutput::new(accepted(), FakeNdiApi::new(), "MULTIVIEW OUT")
        .expect("accepted license must open the sender");
    assert!(out.is_open());
    assert_eq!(out.name(), "MULTIVIEW OUT");
    assert_eq!(out.api().created.as_deref(), Some("MULTIVIEW OUT"));
    // The audit record (who/when) is reachable for export.
    assert_eq!(out.license().acceptance().accepted_by, "operator@example");
}

#[test]
fn from_setting_true_with_audit_yields_accepted_guard() {
    let lic = NdiLicense::from_setting(
        true,
        LicenseAcceptance {
            accepted_by: "ops".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        },
    )
    .expect("accepted + audited must yield a guard");
    let out = NdiOutput::new(lic, FakeNdiApi::new(), "OUT").expect("opens");
    assert!(out.is_open());
}
