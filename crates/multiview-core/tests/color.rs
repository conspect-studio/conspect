//! Integration tests for `ColorInfo::resolve_defaults`.
//!
//! Pins the player-style untagged-default policy (invariant #8 detect step,
//! ADR-C002 / color-management.md §3.2): when a source does not signal its color
//! axes, infer matrix/primaries from geometry as libplacebo/mpv would —
//! `width >= 1280 OR height > 576` => BT.709 (HD **and** UHD), `height == 576`
//! => BT.601 625-line (PAL), `height == 480`/`486` => BT.601 525-line (NTSC),
//! otherwise BT.709 — with transfer ALWAYS BT.709 for untagged SDR video and
//! range limited. It NEVER auto-promotes to BT.2020/PQ/HLG from resolution, and
//! NEVER overwrites an axis the source DID signal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

#[test]
fn ntsc_525_resolution_defaults_to_bt601() {
    // 720x480 (NTSC SD) -> BT.601 525-line primaries + BT.601 matrix.
    let resolved = ColorInfo::default().resolve_defaults(720, 480);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt601_525);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt601);
    // Transfer is always BT.709 for untagged SDR video, even at SD.
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
    // Range defaults to limited for untagged video.
    assert_eq!(resolved.range, ColorRange::Limited);
}

#[test]
fn ntsc_486_resolution_defaults_to_bt601() {
    // 720x486 (SMPTE-170M 525-line) -> BT.601 525-line.
    let resolved = ColorInfo::default().resolve_defaults(720, 486);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt601_525);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt601);
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
}

#[test]
fn pal_576_resolution_defaults_to_bt601_625() {
    // 720x576 (PAL SD) -> BT.601 625-line primaries + BT.601 matrix.
    let resolved = ColorInfo::default().resolve_defaults(720, 576);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt601_625);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt601);
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
    assert_eq!(resolved.range, ColorRange::Limited);
}

#[test]
fn hd_resolution_defaults_to_bt709() {
    // 1920x1080 -> BT.709 on all of matrix/primaries/transfer.
    let resolved = ColorInfo::default().resolve_defaults(1920, 1080);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
    assert_eq!(resolved.range, ColorRange::Limited);
}

#[test]
fn hd_720_lines_is_bt709() {
    // 1280x720 (HD ready): width >= 1280 -> BT.709.
    let resolved = ColorInfo::default().resolve_defaults(1280, 720);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
}

#[test]
fn wide_but_low_lines_is_bt709() {
    // width >= 1280 wins even at a low line count (e.g. 1280x540 anamorphic).
    let resolved = ColorInfo::default().resolve_defaults(1280, 540);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
}

#[test]
fn tall_above_576_is_bt709() {
    // height > 576 with a narrow width (e.g. 480x600) still classifies BT.709.
    let resolved = ColorInfo::default().resolve_defaults(480, 600);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
}

#[test]
fn uhd_resolution_defaults_to_bt709_never_bt2020() {
    // 3840x2160 (4K): the policy must NOT auto-promote to BT.2020/PQ/HLG.
    // libplacebo guesses BT.709 even for 4K; so do we (ADR-C002).
    let resolved = ColorInfo::default().resolve_defaults(3840, 2160);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
    assert_eq!(resolved.range, ColorRange::Limited);
}

#[test]
fn eight_k_resolution_defaults_to_bt709_never_bt2020() {
    // 7680x4320 (8K): still BT.709 — resolution NEVER implies wide gamut/HDR.
    let resolved = ColorInfo::default().resolve_defaults(7680, 4320);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
}

#[test]
fn no_resolution_ever_yields_bt2020_pq_or_hlg() {
    // Exhaustively sweep a wide spread of geometries (including 4K/8K and odd
    // line counts) and assert the heuristic never invents BT.2020/PQ/HLG from
    // resolution alone — the catastrophic SDR->HDR mis-promotion ADR-C002 bans.
    let geometries = [
        (0, 0),
        (320, 240),
        (640, 480),
        (720, 480),
        (720, 486),
        (720, 576),
        (1024, 576),
        (1280, 720),
        (1920, 1080),
        (2560, 1440),
        (3840, 2020),
        (3840, 2160),
        (4096, 2160),
        (7680, 4320),
        (480, 600),
        (1280, 200),
    ];
    for (w, h) in geometries {
        let resolved = ColorInfo::default().resolve_defaults(w, h);
        assert_ne!(
            resolved.primaries,
            ColorPrimaries::Bt2020,
            "resolution {w}x{h} must not yield BT.2020 primaries"
        );
        assert_ne!(
            resolved.matrix,
            MatrixCoefficients::Bt2020Ncl,
            "resolution {w}x{h} must not yield BT.2020 matrix"
        );
        assert_ne!(
            resolved.transfer,
            TransferCharacteristic::Pq,
            "resolution {w}x{h} must not yield PQ transfer"
        );
        assert_ne!(
            resolved.transfer,
            TransferCharacteristic::Hlg,
            "resolution {w}x{h} must not yield HLG transfer"
        );
        assert_ne!(
            resolved.transfer,
            TransferCharacteristic::Bt2020,
            "resolution {w}x{h} must not yield BT.2020 transfer"
        );
    }
}

#[test]
fn signalled_bt2020_primaries_are_preserved() {
    // A source that genuinely signals BT.2020 primaries but leaves the rest
    // unspecified, shown at HD size: the signalled gamut is preserved untouched
    // (we must pass through real wide-gamut sources), the rest fill from policy.
    let signalled = ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Unspecified,
        matrix: MatrixCoefficients::Unspecified,
        range: ColorRange::Unspecified,
    };
    let resolved = signalled.resolve_defaults(1920, 1080);
    // Preserved.
    assert_eq!(resolved.primaries, ColorPrimaries::Bt2020);
    // Filled from policy (BT.709 / limited).
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
    assert_eq!(resolved.range, ColorRange::Limited);
}

#[test]
fn signalled_hdr_axes_are_preserved() {
    // A genuinely-tagged HDR source (BT.2020 + PQ + BT.2020 matrix), shown at SD
    // size, must pass through untouched — the heuristic must never clobber a
    // signalled HDR axis with the SDR default.
    let signalled = ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    };
    let resolved = signalled.resolve_defaults(720, 480);
    assert_eq!(resolved, signalled);
}

#[test]
fn signalled_full_range_is_preserved() {
    let signalled = ColorInfo {
        range: ColorRange::Full,
        ..ColorInfo::default()
    };
    let resolved = signalled.resolve_defaults(720, 480);
    assert_eq!(resolved.range, ColorRange::Full);
    // The rest still come from the SD/525-line policy.
    assert_eq!(resolved.primaries, ColorPrimaries::Bt601_525);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt601);
}

#[test]
fn fully_signalled_info_is_unchanged() {
    let signalled = ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Full,
    };
    let resolved = signalled.resolve_defaults(640, 480);
    assert_eq!(resolved, signalled);
}

#[test]
fn zero_height_falls_back_to_bt709() {
    // Degenerate input must not panic; unknown geometry falls back to the
    // libplacebo default (BT.709), never SD-by-default and never wide gamut.
    let resolved = ColorInfo::default().resolve_defaults(0, 0);
    assert_eq!(resolved.primaries, ColorPrimaries::Bt709);
    assert_eq!(resolved.matrix, MatrixCoefficients::Bt709);
    assert_eq!(resolved.transfer, TransferCharacteristic::Bt709);
}
