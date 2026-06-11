//! Quantization-range expansion and compression (limited <-> full), 8-bit.
//!
//! This is step 1 of the fixed color pipeline (invariant #8): range expansion
//! happens in **code-value space, before** the YUV->RGB matrix. Black maps to
//! `0.0` and white to `1.0`; chroma is centered on `0.0` in the range
//! `[-0.5, 0.5]`. Exactly **one** expansion happens on input and **one**
//! compression on output (color-management.md §2 rule 6, §4.2).
//!
//! All functions are pure and operate on 8-bit samples. The expand functions
//! return normalized `f32`; the compress functions quantize a normalized `f32`
//! back to an 8-bit sample (round-half-away-from-zero, then clamp).

use multiview_core::color::ColorRange;

use crate::error::{Error, Result};

/// Quantize a normalized value in `[lo, hi]`-ish to an 8-bit code value.
///
/// Rounds to the nearest integer (ties away from zero) and clamps to
/// `0..=255`. Used by the range-compression helpers; not part of the public
/// math but shared by luma/chroma compression.
fn quantize_u8(value: f32) -> u8 {
    // round() gives ties-away-from-zero; clamp to the representable byte range
    // BEFORE the integer cast so the conversion can never wrap or saturate
    // incorrectly. The cast is unavoidable for float->int; it is provably in
    // range here because of the preceding clamp.
    let rounded = value.round().clamp(0.0, 255.0);
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    // reason: `rounded` is clamped to 0.0..=255.0 and is integral after
    // `round()`, so the truncating cast to u8 is exact and lossless.
    let byte = rounded as u8;
    byte
}

/// Expand an 8-bit **luma** sample to a normalized value (black -> 0.0,
/// white -> 1.0) for the given quantization `range`.
///
/// - Limited: `(Y8 - 16) / 219` (8-bit `16..=235`).
/// - Full: `Y8 / 255`.
///
/// Values outside the nominal range are **not** clamped — head/foot-room is
/// preserved (it may carry valid super-black/super-white that later stages
/// handle), matching libplacebo's normalize-then-process model.
#[must_use]
pub fn expand_luma(y8: u8, range: ColorRange) -> f32 {
    let y = f32::from(y8);
    match range {
        ColorRange::Full => y / 255.0,
        // Limited, Unspecified, and any future `#[non_exhaustive]` variant all
        // take the broadcast-video (limited) path: detection
        // (resolve_defaults) is expected to have run, but defaulting to limited
        // is the safe choice and keeps the function total.
        _ => (y - 16.0) / 219.0,
    }
}

/// Expand an 8-bit **chroma** sample to a normalized value centered on `0.0`
/// in `[-0.5, 0.5]` for the given quantization `range`.
///
/// - Limited: `(C8 - 128) / 224` (8-bit `16..=240`).
/// - Full: `(C8 - 128) / 255`.
#[must_use]
pub fn expand_chroma(c8: u8, range: ColorRange) -> f32 {
    let c = f32::from(c8) - 128.0;
    match range {
        ColorRange::Full => c / 255.0,
        // Limited / Unspecified / future variants -> limited path.
        _ => c / 224.0,
    }
}

/// Compress a normalized **luma** value (black 0.0 -> white 1.0) back to an
/// 8-bit code value for the given quantization `range` (inverse of
/// [`expand_luma`]).
///
/// - Limited: `round(Y * 219 + 16)`.
/// - Full: `round(Y * 255)`.
///
/// The result is clamped to `0..=255`.
#[must_use]
pub fn compress_luma(y: f32, range: ColorRange) -> u8 {
    let code = match range {
        ColorRange::Full => y * 255.0,
        // Limited / Unspecified / future variants -> limited path.
        _ => y.mul_add(219.0, 16.0),
    };
    quantize_u8(code)
}

/// Compress a normalized **chroma** value (`[-0.5, 0.5]`, centered 0.0) back to
/// an 8-bit code value for the given quantization `range` (inverse of
/// [`expand_chroma`]).
///
/// - Limited: `round(C * 224 + 128)`.
/// - Full: `round(C * 255 + 128)`.
///
/// The result is clamped to `0..=255`.
#[must_use]
pub fn compress_chroma(c: f32, range: ColorRange) -> u8 {
    let code = match range {
        ColorRange::Full => c.mul_add(255.0, 128.0),
        // Limited / Unspecified / future variants -> limited path.
        _ => c.mul_add(224.0, 128.0),
    };
    quantize_u8(code)
}

/// Validate that a [`ColorRange`] axis has been resolved (is not
/// [`ColorRange::Unspecified`]).
///
/// The kernel must never see an unspecified axis; detection
/// ([`multiview_core::color::ColorInfo::resolve_defaults`]) runs first.
///
/// # Errors
///
/// Returns [`Error::UnresolvedColor`] when `range` is
/// [`ColorRange::Unspecified`].
pub fn require_resolved(range: ColorRange) -> Result<ColorRange> {
    match range {
        ColorRange::Unspecified => Err(Error::UnresolvedColor("range")),
        resolved => Ok(resolved),
    }
}
