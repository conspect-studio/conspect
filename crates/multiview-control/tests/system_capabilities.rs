//! `GET /api/v1/system/capabilities` (ADR-W030): the honest default-build
//! capability + licence surface, viewer-readable, plus the compliance-critical
//! effective-licence mapping (AGENTS.md §G / ADR-0012).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_control::system::{BuildInfo, EffectiveLicense};
use support::{body_json, get, harness, send, VIEWER_TOKEN};

// --- Licence mapping: the compliance surface (per feature-combo) ---------------

#[test]
fn default_profile_is_lgpl_clean_and_redistributable() {
    let build = BuildInfo::resolve(false, false, vec!["software".to_owned()]);
    assert_eq!(build.effective_license, EffectiveLicense::LgplClean);
    assert!(build.redistributable);
    assert!(!build.ndi);
}

#[test]
fn gpl_codecs_profile_is_gpl_and_still_redistributable() {
    // `gpl-codecs` pulls x264/x265 → the whole product is GPL (AGENTS.md §G /
    // ADR-0012). GPL is redistributable (under copyleft), so `redistributable`
    // stays true; only the licence string changes.
    let build = BuildInfo::resolve(true, false, vec!["gpl-codecs".to_owned()]);
    assert_eq!(build.effective_license, EffectiveLicense::Gpl);
    assert!(build.redistributable);
}

#[test]
fn ndi_feature_sets_the_ndi_flag_without_changing_the_codec_licence() {
    // NDI is runtime-loaded (never vendored), so it does not change the
    // codec-linking licence and does not make the artifact non-redistributable.
    let build = BuildInfo::resolve(false, true, vec!["ndi".to_owned()]);
    assert!(build.ndi);
    assert_eq!(build.effective_license, EffectiveLicense::LgplClean);
    assert!(build.redistributable);
}

#[test]
fn effective_license_serializes_to_the_exact_compliance_strings() {
    assert_eq!(
        serde_json::to_value(EffectiveLicense::LgplClean).unwrap(),
        serde_json::json!("LGPL-clean")
    );
    assert_eq!(
        serde_json::to_value(EffectiveLicense::Gpl).unwrap(),
        serde_json::json!("GPL")
    );
}

// --- Route + auth --------------------------------------------------------------

#[tokio::test]
async fn viewer_reads_the_default_capability_surface() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/system/capabilities", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let backends = body["backends"].as_array().expect("backends array");
    assert!(!backends.is_empty());
    // The software path is always available (the universal fallback).
    assert!(backends
        .iter()
        .any(|b| b["kind"] == "software" && b["available"] == true));

    // The build/compliance surface is inline (ADR-W030: `build{}` inline).
    assert!(body["build"]["effective_license"].is_string());
    assert_eq!(body["build"]["redistributable"], true);

    // The compositor classification is present.
    assert!(body["compositor"]["class"].is_string());

    // A default build (no `ndi` feature) omits the attribution.
    assert!(body.get("ndi_attribution").is_none() || body["ndi_attribution"].is_null());
}

#[tokio::test]
async fn a_bad_bearer_is_rejected_401() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/system/capabilities", "bad-key.bad-secret"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
