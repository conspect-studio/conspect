//! Integration tests for the transfer functions (invariant #8 step 3):
//! EOTF (code -> linear) and OETF/inverse (linear -> code) for sRGB, BT.709
//! (BT.1886 display), the BT.709 camera OETF, PQ ST 2084, and HLG ARIB B67.
//! Pins endpoints, a documented anchor, and EOTF<->OETF round-trip tolerance
//! (color-management.md §4.4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::transfer::{
    bt709_camera_oetf, bt709_camera_oetf_inverse, bt709_eotf, bt709_oetf_inverse, eotf, hlg_eotf,
    hlg_oetf, oetf, pq_eotf, pq_oetf, srgb_eotf, srgb_oetf,
};
use multiview_core::color::TransferCharacteristic;

const EPS: f32 = 1e-4;

fn assert_close(a: f32, b: f32, eps: f32, what: &str) {
    assert!((a - b).abs() < eps, "{what}: {a} vs {b}");
}

#[test]
fn all_transfers_fix_endpoints() {
    // 0 -> 0 and 1 -> 1 for every EOTF and its inverse.
    assert_close(srgb_eotf(0.0), 0.0, EPS, "srgb eotf 0");
    assert_close(srgb_eotf(1.0), 1.0, EPS, "srgb eotf 1");
    assert_close(bt709_eotf(0.0), 0.0, EPS, "bt709 eotf 0");
    assert_close(bt709_eotf(1.0), 1.0, EPS, "bt709 eotf 1");
    assert_close(pq_eotf(0.0), 0.0, EPS, "pq eotf 0");
    assert_close(pq_eotf(1.0), 1.0, EPS, "pq eotf 1");
    assert_close(hlg_oetf(0.0), 0.0, EPS, "hlg oetf 0");
    assert_close(hlg_oetf(1.0), 1.0, EPS, "hlg oetf 1");
}

#[test]
fn srgb_roundtrip_and_threshold() {
    // Round-trip across the curve.
    for i in 0_u8..=100 {
        let c = f32::from(i) / 100.0;
        let back = srgb_oetf(srgb_eotf(c));
        assert_close(back, c, 2e-4, "srgb roundtrip");
    }
    // Linear-segment slope near 0: srgb_eotf(0.04045) ~ 0.0031308.
    assert_close(srgb_eotf(0.040_45), 0.003_130_8, 1e-5, "srgb threshold");
}

#[test]
fn bt709_display_roundtrip_is_pure_24_gamma() {
    // BT.1886 display EOTF modeled as pure 2.4 power: eotf(0.5) = 0.5^2.4.
    let expected = 0.5_f32.powf(2.4);
    assert_close(bt709_eotf(0.5), expected, EPS, "bt709 eotf 0.5");
    for i in 0_u8..=100 {
        let c = f32::from(i) / 100.0;
        assert_close(
            bt709_oetf_inverse(bt709_eotf(c)),
            c,
            2e-4,
            "bt709 roundtrip",
        );
    }
}

#[test]
fn bt709_camera_oetf_distinct_from_display_and_roundtrips() {
    // The camera OETF is NOT the display EOTF: at linear 0.5 they differ.
    let cam = bt709_camera_oetf(0.5);
    let disp_inv = bt709_oetf_inverse(0.5);
    assert!(
        (cam - disp_inv).abs() > 0.01,
        "camera OETF must differ from display inverse-EOTF: {cam} vs {disp_inv}"
    );
    // And it must round-trip with its own inverse.
    for i in 0_u8..=100 {
        let l = f32::from(i) / 100.0;
        assert_close(
            bt709_camera_oetf_inverse(bt709_camera_oetf(l)),
            l,
            2e-4,
            "bt709 camera roundtrip",
        );
    }
}

#[test]
fn pq_roundtrip_and_known_anchor() {
    // Round-trip across the curve.
    for i in 0_u8..=100 {
        let e = f32::from(i) / 100.0;
        assert_close(pq_oetf(pq_eotf(e)), e, 3e-4, "pq roundtrip");
    }
    // 100 cd/m^2 is linear 0.01 (1.0 == 10000 nits); its PQ code is ~0.5081.
    let code_for_100nits = pq_oetf(0.01);
    assert_close(code_for_100nits, 0.508_078_4, 5e-4, "pq 100-nit anchor");
    // And SDR diffuse white ~203 nits -> ~0.58 PQ (color-management.md §5.1).
    let code_for_203 = pq_oetf(0.020_3);
    assert!(
        (0.56..0.60).contains(&code_for_203),
        "203-nit PQ code = {code_for_203}"
    );
}

#[test]
fn hlg_roundtrip_and_join_point() {
    for i in 0_u8..=100 {
        let l = f32::from(i) / 100.0;
        assert_close(hlg_eotf(hlg_oetf(l)), l, 3e-4, "hlg roundtrip");
    }
    // Join point: L = 1/12 -> V = sqrt(3 * 1/12) = 0.5.
    assert_close(hlg_oetf(1.0 / 12.0), 0.5, EPS, "hlg join point");
}

#[test]
fn dispatch_matches_concrete_functions() {
    // The axis-dispatch eotf/oetf must agree with the concrete functions.
    assert_close(
        eotf(0.5, TransferCharacteristic::Bt709).unwrap(),
        bt709_eotf(0.5),
        EPS,
        "dispatch bt709 eotf",
    );
    assert_close(
        eotf(0.5, TransferCharacteristic::Srgb).unwrap(),
        srgb_eotf(0.5),
        EPS,
        "dispatch srgb eotf",
    );
    assert_close(
        eotf(0.5, TransferCharacteristic::Pq).unwrap(),
        pq_eotf(0.5),
        EPS,
        "dispatch pq eotf",
    );
    assert_close(
        oetf(0.5, TransferCharacteristic::Hlg).unwrap(),
        hlg_oetf(0.5),
        EPS,
        "dispatch hlg oetf",
    );
    // BT.601 and BT.2020 SDR transfers route to the BT.1886 display curve.
    assert_close(
        eotf(0.5, TransferCharacteristic::Bt601).unwrap(),
        bt709_eotf(0.5),
        EPS,
        "dispatch bt601 -> bt1886",
    );
}

#[test]
fn dispatch_rejects_unspecified_transfer() {
    assert!(eotf(0.5, TransferCharacteristic::Unspecified).is_err());
    assert!(oetf(0.5, TransferCharacteristic::Unspecified).is_err());
}
