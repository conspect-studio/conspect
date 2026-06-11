//! Gamut (primaries) conversion in **linear light**, via the XYZ connection
//! space.
//!
//! Step 4 of the fixed pipeline (invariant #8): primaries conversion is done on
//! **linear** RGB (after the EOTF), separately from the YUV matrix — never
//! matrix-YUV between primaries (color-management.md §2 rule 4, §4.5).
//!
//! For a source gamut and a canvas gamut, the conversion is
//! `M = M_canvas^-1 . M_source`, where each `M_x` is that gamut's normalized
//! primary matrix (RGB->XYZ, D65 white). The matrices are **derived** here from
//! the published chromaticities (a 3x3 solve), not hard-coded, so they stay
//! honest; the tests pin them against the BT.2087 reference values. If source
//! and canvas share the same primaries this is the identity and is skipped.

use multiview_core::color::ColorPrimaries;

use crate::error::{Error, Result};

/// A row-major 3x3 matrix of `f64` (color math runs in `f64`, results narrow to
/// `f32` at the channel boundary).
pub type Mat3 = [[f64; 3]; 3];

/// CIE xy chromaticities `[(xr, yr), (xg, yg), (xb, yb)]` of a gamut's
/// primaries plus its white point `(xw, yw)`.
struct Chromaticities {
    primaries: [(f64, f64); 3],
    white: (f64, f64),
}

/// D65 white point (BT.709 / BT.601 / BT.2020 all use D65).
const D65: (f64, f64) = (0.312_7, 0.329_0);

/// Return the published chromaticities for a supported primaries axis, or
/// [`None`] for axes with no gamut (Unspecified / Rgb-only would never reach
/// here).
fn chromaticities(primaries: ColorPrimaries) -> Option<Chromaticities> {
    match primaries {
        ColorPrimaries::Bt709 => Some(Chromaticities {
            primaries: [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)],
            white: D65,
        }),
        ColorPrimaries::Bt2020 => Some(Chromaticities {
            primaries: [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)],
            white: D65,
        }),
        ColorPrimaries::Bt601_525 => Some(Chromaticities {
            // SMPTE 170M / 240M (NTSC) primaries.
            primaries: [(0.630, 0.340), (0.310, 0.595), (0.155, 0.070)],
            white: D65,
        }),
        ColorPrimaries::Bt601_625 => Some(Chromaticities {
            // BT.470 System B/G (PAL/SECAM) primaries.
            primaries: [(0.640, 0.330), (0.290, 0.600), (0.150, 0.060)],
            white: D65,
        }),
        _ => None,
    }
}

/// XYZ of a chromaticity at unit luminance: `(x/y, 1, (1-x-y)/y)`.
const fn xyz(x: f64, y: f64) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Invert a 3x3 matrix via the adjugate / determinant.
///
/// Returns [`None`] when the matrix is singular (determinant `0`); the primary
/// matrices here are always non-singular, so this only guards against a
/// degenerate custom gamut.
fn invert(m: Mat3) -> Option<Mat3> {
    let [[m00, m01, m02], [m10, m11, m12], [m20, m21, m22]] = m;
    let cof00 = m11 * m22 - m12 * m21;
    let cof01 = -(m10 * m22 - m12 * m20);
    let cof02 = m10 * m21 - m11 * m20;
    let det = m00 * cof00 + m01 * cof01 + m02 * cof02;
    if det == 0.0 {
        return None;
    }
    let cof10 = -(m01 * m22 - m02 * m21);
    let cof11 = m00 * m22 - m02 * m20;
    let cof12 = -(m00 * m21 - m01 * m20);
    let cof20 = m01 * m12 - m02 * m11;
    let cof21 = -(m00 * m12 - m02 * m10);
    let cof22 = m00 * m11 - m01 * m10;
    // Inverse = adjugate (cofactor transpose) / det.
    Some([
        [cof00 / det, cof10 / det, cof20 / det],
        [cof01 / det, cof11 / det, cof21 / det],
        [cof02 / det, cof12 / det, cof22 / det],
    ])
}

/// Multiply two 3x3 matrices (`a . b`).
fn mat_mul(a: Mat3, b: Mat3) -> Mat3 {
    let mut out = [[0.0_f64; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            let mut sum = 0.0;
            for k in 0..3 {
                // Indices are all in 0..3 over a fixed [[_;3];3]; use get to
                // satisfy the indexing-slicing lint without a panic path.
                let ai = a.get(i).and_then(|r| r.get(k)).copied().unwrap_or(0.0);
                let bk = b.get(k).and_then(|r| r.get(j)).copied().unwrap_or(0.0);
                sum += ai * bk;
            }
            *cell = sum;
        }
    }
    out
}

/// Solve `M x = w` for `x` (used to scale the primary columns to the white
/// point). Returns [`None`] if `M` is singular.
fn solve(m: Mat3, w: [f64; 3]) -> Option<[f64; 3]> {
    let inv = invert(m)?;
    Some([
        inv[0][0] * w[0] + inv[0][1] * w[1] + inv[0][2] * w[2],
        inv[1][0] * w[0] + inv[1][1] * w[1] + inv[1][2] * w[2],
        inv[2][0] * w[0] + inv[2][1] * w[1] + inv[2][2] * w[2],
    ])
}

/// The normalized primary matrix (linear RGB -> CIE XYZ) for a gamut.
///
/// Built from the chromaticities by solving for the per-primary luminance
/// scale that maps RGB `(1, 1, 1)` to the gamut's white point.
///
/// # Errors
///
/// Returns [`Error::UnsupportedPrimaries`] for a primaries axis with no defined
/// gamut (Unspecified / Rgb), or if the (degenerate) chromaticities are
/// singular.
pub fn rgb_to_xyz(primaries: ColorPrimaries) -> Result<Mat3> {
    let chroma = chromaticities(primaries).ok_or(Error::UnsupportedPrimaries(primaries))?;
    let [(xr, yr), (xg, yg), (xb, yb)] = chroma.primaries;
    let cr = xyz(xr, yr);
    let cg = xyz(xg, yg);
    let cb = xyz(xb, yb);
    // Columns are the primary XYZ vectors.
    let m = [
        [cr[0], cg[0], cb[0]],
        [cr[1], cg[1], cb[1]],
        [cr[2], cg[2], cb[2]],
    ];
    let w = xyz(chroma.white.0, chroma.white.1);
    let s = solve(m, w).ok_or(Error::UnsupportedPrimaries(primaries))?;
    Ok([
        [m[0][0] * s[0], m[0][1] * s[1], m[0][2] * s[2]],
        [m[1][0] * s[0], m[1][1] * s[1], m[1][2] * s[2]],
        [m[2][0] * s[0], m[2][1] * s[1], m[2][2] * s[2]],
    ])
}

/// The linear-light gamut conversion matrix `source -> canvas`
/// (`M_canvas^-1 . M_source`).
///
/// Returns [`None`] wrapped as `Ok(None)` semantics is avoided; instead, when
/// `source == canvas` the identity is returned (the caller may skip the
/// multiply). The matrix maps a linear RGB triple in the source gamut to linear
/// RGB in the canvas gamut.
///
/// # Errors
///
/// Returns [`Error::UnsupportedPrimaries`] when either gamut is undefined.
pub fn convert_matrix(source: ColorPrimaries, canvas: ColorPrimaries) -> Result<Mat3> {
    if source == canvas {
        return Ok(IDENTITY);
    }
    let m_source = rgb_to_xyz(source)?;
    let m_canvas = rgb_to_xyz(canvas)?;
    let inv_canvas = invert(m_canvas).ok_or(Error::UnsupportedPrimaries(canvas))?;
    Ok(mat_mul(inv_canvas, m_source))
}

/// The 3x3 identity matrix.
pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Apply a 3x3 matrix to a linear RGB triple, returning `f32` working values.
#[must_use]
pub fn apply(m: Mat3, rgb: [f32; 3]) -> [f32; 3] {
    let r = f64::from(rgb[0]);
    let g = f64::from(rgb[1]);
    let b = f64::from(rgb[2]);
    [
        demote(m[0][0] * r + m[0][1] * g + m[0][2] * b),
        demote(m[1][0] * r + m[1][1] * g + m[1][2] * b),
        demote(m[2][0] * r + m[2][1] * g + m[2][2] * b),
    ]
}

/// Narrow an `f64` gamut intermediate to `f32` working precision.
fn demote(value: f64) -> f32 {
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    // reason: deliberate, documented narrowing of an f64 color intermediate to
    // the f32 working precision; `as` is the only `f64 -> f32` conversion.
    let narrowed = value as f32;
    narrowed
}
