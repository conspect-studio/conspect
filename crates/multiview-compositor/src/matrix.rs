//! YUV' <-> R'G'B' matrices for BT.601 / BT.709 / BT.2020-NCL.
//!
//! Step 2 of the fixed pipeline (invariant #8): the matrix operates on
//! **gamma-encoded** R'G'B' (NOT linear), **after** range expansion. Inputs are
//! normalized luma `Y in [0, 1]` and centered chroma `Cb, Cr in [-0.5, 0.5]`;
//! outputs are gamma-encoded R'G'B'. Coefficients are derived from the
//! per-system luma weights `(Kr, Kg, Kb)` and carried at full `f64` precision
//! to avoid drift across many tiles (color-management.md §4.3).
//!
//! The matrix axis is independent of the primaries axis: both BT.601 525-line
//! and 625-line share the single BT.601 matrix here; the gamut difference lives
//! in [`crate::primaries`].

use multiview_core::color::MatrixCoefficients;

use crate::error::{Error, Result};

/// Per-system luma weights `(Kr, Kg, Kb)`; `Kg = 1 - Kr - Kb`.
///
/// Returns the exact ITU-R weights for BT.601 / BT.709 / BT.2020. The
/// [`MatrixCoefficients::Rgb`] identity and `Unspecified` variants have no luma
/// weights and yield [`None`].
#[must_use]
pub fn luma_weights(matrix: MatrixCoefficients) -> Option<(f64, f64, f64)> {
    match matrix {
        MatrixCoefficients::Bt601 => Some((0.299, 0.587, 0.114)),
        MatrixCoefficients::Bt709 => Some((0.2126, 0.7152, 0.0722)),
        MatrixCoefficients::Bt2020Ncl => Some((0.2627, 0.6780, 0.0593)),
        _ => None,
    }
}

/// Convert normalized `YUV'` to gamma-encoded `R'G'B'` for the given matrix.
///
/// Uses the general derivation
/// `R' = Y + 2(1-Kr)Cr`,
/// `B' = Y + 2(1-Kb)Cb`,
/// `G' = Y - (2 Kr (1-Kr)/Kg) Cr - (2 Kb (1-Kb)/Kg) Cb`,
/// evaluated at full `f64` precision so the constants match
/// color-management.md §4.3 to the documented digits (e.g. BT.2020-NCL
/// `G'` uses `0.16455312684366` / `0.57135312684366`).
///
/// `y` is normalized luma in `[0, 1]`; `cb`/`cr` are centered chroma in
/// `[-0.5, 0.5]`. The returned `[R', G', B']` are gamma-encoded (still need the
/// EOTF to linearize).
///
/// # Errors
///
/// Returns [`Error::UnsupportedMatrix`] when `matrix` is
/// [`MatrixCoefficients::Unspecified`] or [`MatrixCoefficients::Rgb`] (the
/// latter means "samples are already RGB"; the caller must not invoke a YUV
/// matrix).
pub fn yuv_to_rgb(y: f32, cb: f32, cr: f32, matrix: MatrixCoefficients) -> Result<[f32; 3]> {
    let (kr, kg, kb) = luma_weights(matrix).ok_or(Error::UnsupportedMatrix(matrix))?;
    let y = f64::from(y);
    let cb = f64::from(cb);
    let cr = f64::from(cr);

    let r = y + 2.0 * (1.0 - kr) * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let g = y - (2.0 * kr * (1.0 - kr) / kg) * cr - (2.0 * kb * (1.0 - kb) / kg) * cb;

    Ok([demote(r), demote(g), demote(b)])
}

/// Convert gamma-encoded `R'G'B'` to normalized `YUV'` for the given matrix
/// (inverse of [`yuv_to_rgb`]).
///
/// `Y = Kr R' + Kg G' + Kb B'`,
/// `Cb = (B' - Y) / (2 (1 - Kb))`,
/// `Cr = (R' - Y) / (2 (1 - Kr))`.
///
/// Returns `[Y, Cb, Cr]` with `Y in [0, 1]` and `Cb, Cr in [-0.5, 0.5]` for
/// in-gamut inputs.
///
/// # Errors
///
/// Returns [`Error::UnsupportedMatrix`] for the same cases as [`yuv_to_rgb`].
pub fn rgb_to_yuv(r: f32, g: f32, b: f32, matrix: MatrixCoefficients) -> Result<[f32; 3]> {
    let (kr, kg, kb) = luma_weights(matrix).ok_or(Error::UnsupportedMatrix(matrix))?;
    let r = f64::from(r);
    let g = f64::from(g);
    let b = f64::from(b);

    let y = kr * r + kg * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));

    Ok([demote(y), demote(cb), demote(cr)])
}

/// Narrow an `f64` intermediate to the `f32` working precision.
///
/// The matrix math runs in `f64` to honor the full-precision-constants rule,
/// then results return to the `f32` working space.
fn demote(value: f64) -> f32 {
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    // reason: deliberate, documented narrowing of an f64 color intermediate to
    // the f32 working precision; `as` is the only `f64 -> f32` conversion and
    // the rounding is the intended behavior.
    let narrowed = value as f32;
    narrowed
}
