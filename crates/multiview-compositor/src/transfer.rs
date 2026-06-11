//! Transfer functions: EOTF (code -> linear) and OETF/inverse-EOTF
//! (linear -> code) for sRGB, BT.709, PQ (ST 2084), and HLG (ARIB STD-B67).
//!
//! Step 3 of the fixed pipeline (invariant #8): linearize each tile via its own
//! EOTF **before** any primaries math, scaling, or blending; apply the canvas
//! OETF once on the encode side. The functions here are scalar and pure;
//! the compositor applies them per channel.
//!
//! **EOTF != OETF.** The BT.709 *OETF* (camera curve) is not its display EOTF
//! (BT.1886, ~pure 2.4) and is not sRGB — see color-management.md §4.4. For
//! display-referred SDR *decode* this module exposes [`bt709_eotf`] /
//! [`bt709_oetf_inverse`] as the BT.1886 display curve (the curve actually used
//! to linearize broadcast/capture video), plus the genuine camera
//! [`bt709_camera_oetf`] / [`bt709_camera_oetf_inverse`] pair for completeness.
//!
//! PQ is normalized so linear `1.0 == 10000 cd/m^2`. HLG here is the bare
//! ARIB STD-B67 OETF/inverse-OETF (scene-referred); the display OOTF/system
//! gamma is **not** applied (color-management.md §4.4 notes a naive round-trip
//! is not display-correct — the OOTF is a separate, later concern).

use multiview_core::color::TransferCharacteristic;

use crate::error::{Error, Result};

/// The BT.1886 / pure-2.4 display gamma used as the SDR video EOTF.
const DISPLAY_GAMMA: f64 = 2.4;

// --- sRGB (graphics / RGB sources) -----------------------------------------

/// sRGB EOTF (code value -> linear light).
///
/// `L = C / 12.92` for `C <= 0.04045`, else `((C + 0.055) / 1.055)^2.4`.
#[must_use]
pub fn srgb_eotf(c: f32) -> f32 {
    let c = f64::from(c);
    let l = if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    };
    demote(l)
}

/// sRGB inverse-EOTF (linear light -> code value).
///
/// `C = 12.92 L` for `L <= 0.0031308`, else `1.055 L^(1/2.4) - 0.055`.
#[must_use]
pub fn srgb_oetf(l: f32) -> f32 {
    let l = f64::from(l);
    let c = if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    demote(c)
}

// --- BT.709 display EOTF (BT.1886, pure 2.4) --------------------------------

/// BT.1886 display EOTF (code value -> linear) used to decode SDR BT.709 video.
///
/// Modeled as the pure power law `L = C^2.4` (the BT.1886 reference with zero
/// black level), which is the display curve a correct SDR player applies — the
/// curve to linearize broadcast/capture video with. Negative inputs are passed
/// through with sign preserved so head/foot-room normalized values do not
/// produce NaNs.
#[must_use]
pub fn bt709_eotf(c: f32) -> f32 {
    demote(signed_pow(f64::from(c), DISPLAY_GAMMA))
}

/// Inverse of [`bt709_eotf`] (linear -> code): `C = L^(1/2.4)`.
#[must_use]
pub fn bt709_oetf_inverse(l: f32) -> f32 {
    demote(signed_pow(f64::from(l), 1.0 / DISPLAY_GAMMA))
}

// --- BT.709 camera OETF (scene-referred capture curve) ----------------------

/// BT.709 camera OETF (linear light -> code value).
///
/// `V = 4.5 L` for `L < beta`, else `alpha L^0.45 - (alpha - 1)`, with the
/// precise constants `alpha = 1.099_296_826_809_44`,
/// `beta = 0.018_053_968_510_807`. This is the *capture* curve and is distinct
/// from the display EOTF ([`bt709_eotf`]) and from sRGB.
#[must_use]
pub fn bt709_camera_oetf(l: f32) -> f32 {
    const ALPHA: f64 = 1.099_296_826_809_44;
    const BETA: f64 = 0.018_053_968_510_807;
    let l = f64::from(l);
    let v = if l < BETA {
        4.5 * l
    } else {
        ALPHA * l.powf(0.45) - (ALPHA - 1.0)
    };
    demote(v)
}

/// Inverse of [`bt709_camera_oetf`] (code value -> linear light).
#[must_use]
pub fn bt709_camera_oetf_inverse(v: f32) -> f32 {
    const ALPHA: f64 = 1.099_296_826_809_44;
    const BETA: f64 = 0.018_053_968_510_807;
    let v = f64::from(v);
    // Threshold on V at the join point V = 4.5 * BETA.
    let l = if v < 4.5 * BETA {
        v / 4.5
    } else {
        ((v + (ALPHA - 1.0)) / ALPHA).powf(1.0 / 0.45)
    };
    demote(l)
}

// --- PQ / SMPTE ST 2084 -----------------------------------------------------

const PQ_M1: f64 = 0.159_301_757_812_5; // 2610 / 16384
const PQ_M2: f64 = 78.843_75; // 2523 / 32 * 16
const PQ_C1: f64 = 0.835_937_5; // 3424 / 4096
const PQ_C2: f64 = 18.851_562_5; // 2413 / 4096 * 32
const PQ_C3: f64 = 18.687_5; // 2392 / 4096 * 32

/// PQ / ST 2084 EOTF (code value -> linear), normalized so linear `1.0`
/// corresponds to 10000 cd/m^2.
///
/// `L = ( max(E'^(1/m2) - c1, 0) / (c2 - c3 E'^(1/m2)) )^(1/m1)`.
#[must_use]
pub fn pq_eotf(e: f32) -> f32 {
    let e = f64::from(e).clamp(0.0, 1.0);
    let ep = e.powf(1.0 / PQ_M2);
    let num = (ep - PQ_C1).max(0.0);
    let den = PQ_C2 - PQ_C3 * ep;
    let l = if den <= 0.0 {
        0.0
    } else {
        (num / den).powf(1.0 / PQ_M1)
    };
    demote(l)
}

/// PQ / ST 2084 inverse-EOTF (linear -> code value); inverse of [`pq_eotf`].
///
/// `E' = ( (c1 + c2 L^m1) / (1 + c3 L^m1) )^m2`.
#[must_use]
pub fn pq_oetf(l: f32) -> f32 {
    let l = f64::from(l).clamp(0.0, 1.0);
    let lm = l.powf(PQ_M1);
    let e = ((PQ_C1 + PQ_C2 * lm) / (1.0 + PQ_C3 * lm)).powf(PQ_M2);
    demote(e)
}

// --- HLG / ARIB STD-B67 -----------------------------------------------------

const HLG_A: f64 = 0.178_832_77;
const HLG_B: f64 = 0.284_668_92;
const HLG_C: f64 = 0.559_910_73;

/// HLG / ARIB STD-B67 OETF (scene linear -> code value).
///
/// `V = sqrt(3 L)` for `L <= 1/12`, else `a ln(12 L - b) + c`. Scene-referred;
/// the display OOTF/system gamma is **not** applied here.
#[must_use]
pub fn hlg_oetf(l: f32) -> f32 {
    let l = f64::from(l).max(0.0);
    let v = if l <= 1.0 / 12.0 {
        (3.0 * l).sqrt()
    } else {
        HLG_A * (12.0 * l - HLG_B).ln() + HLG_C
    };
    demote(v)
}

/// HLG / ARIB STD-B67 inverse-OETF (code value -> scene linear); inverse of
/// [`hlg_oetf`].
///
/// `L = V^2 / 3` for `V <= 1/2`, else `(exp((V - c) / a) + b) / 12`.
#[must_use]
pub fn hlg_eotf(v: f32) -> f32 {
    let v = f64::from(v).max(0.0);
    let l = if v <= 0.5 {
        v * v / 3.0
    } else {
        (((v - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    };
    demote(l)
}

// --- dispatch by axis -------------------------------------------------------

/// Linearize a gamma-encoded code value via the EOTF selected by `transfer`.
///
/// SDR transfers (BT.709/BT.601/BT.2020 SDR) use the BT.1886 display EOTF
/// ([`bt709_eotf`]); sRGB uses [`srgb_eotf`]; PQ uses [`pq_eotf`]; HLG uses the
/// inverse-OETF [`hlg_eotf`] (scene-referred, no OOTF).
///
/// # Errors
///
/// Returns [`Error::UnsupportedTransfer`] when `transfer` is
/// [`TransferCharacteristic::Unspecified`] (detection must run first) or a
/// future `#[non_exhaustive]` variant this path does not model.
pub fn eotf(c: f32, transfer: TransferCharacteristic) -> Result<f32> {
    match transfer {
        TransferCharacteristic::Bt709
        | TransferCharacteristic::Bt601
        | TransferCharacteristic::Bt2020 => Ok(bt709_eotf(c)),
        TransferCharacteristic::Srgb => Ok(srgb_eotf(c)),
        TransferCharacteristic::Pq => Ok(pq_eotf(c)),
        TransferCharacteristic::Hlg => Ok(hlg_eotf(c)),
        other => Err(Error::UnsupportedTransfer(other)),
    }
}

/// Apply the inverse transfer (linear -> code) selected by `transfer`; inverse
/// of [`eotf`]. This is the canvas OETF on the encode side.
///
/// # Errors
///
/// Returns [`Error::UnsupportedTransfer`] for the same cases as [`eotf`].
pub fn oetf(l: f32, transfer: TransferCharacteristic) -> Result<f32> {
    match transfer {
        TransferCharacteristic::Bt709
        | TransferCharacteristic::Bt601
        | TransferCharacteristic::Bt2020 => Ok(bt709_oetf_inverse(l)),
        TransferCharacteristic::Srgb => Ok(srgb_oetf(l)),
        TransferCharacteristic::Pq => Ok(pq_oetf(l)),
        TransferCharacteristic::Hlg => Ok(hlg_oetf(l)),
        other => Err(Error::UnsupportedTransfer(other)),
    }
}

/// `value^exp` with the sign of `value` preserved (so negative head/foot-room
/// normalized values produce a real result rather than NaN).
fn signed_pow(value: f64, exp: f64) -> f64 {
    if value < 0.0 {
        -((-value).powf(exp))
    } else {
        value.powf(exp)
    }
}

/// Narrow an `f64` transfer-function intermediate to `f32` working precision.
fn demote(value: f64) -> f32 {
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    // reason: deliberate, documented narrowing of an f64 color intermediate to
    // the f32 working precision; `as` is the only `f64 -> f32` conversion.
    let narrowed = value as f32;
    narrowed
}
