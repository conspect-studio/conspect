//! GPU uniform buffers and the CPU-side derivation of their contents.
//!
//! The matrices and range scale/offset handed to the shaders are derived here
//! by the **same** pure-Rust modules the CPU reference uses
//! ([`crate::matrix`], [`crate::primaries`], [`crate::range`]), so the GPU
//! pipeline diverges from the oracle only by f32-vs-f64 intermediate precision
//! and GPU transcendental accuracy — covered by the SSIM/PSNR threshold.
//!
//! Layouts mirror the `struct`s in the WGSL: every field is `vec4`-aligned so
//! the std140 uniform / storage rules are satisfied without manual padding
//! arithmetic at the call site.

use bytemuck::{Pod, Zeroable};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

use crate::error::{Error, Result};
use crate::pipeline::CanvasColor;
use crate::{matrix, primaries, range};

/// Transfer-function id shared with `common.wgsl` (the `switch` in `eotf`/
/// `oetf`). Default (BT.1886) is `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferId {
    /// BT.709 / BT.601 / BT.2020 SDR display EOTF (BT.1886, pure 2.4).
    Bt1886 = 0,
    /// sRGB.
    Srgb = 1,
    /// SMPTE ST 2084 (PQ).
    Pq = 2,
    /// ARIB STD-B67 (HLG).
    Hlg = 3,
}

impl TransferId {
    /// Map a [`TransferCharacteristic`] to the shader id, mirroring the dispatch
    /// in [`crate::transfer::eotf`] / [`crate::transfer::oetf`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedTransfer`] for [`TransferCharacteristic::Unspecified`]
    /// or any variant the shader does not model.
    pub fn from_transfer(transfer: TransferCharacteristic) -> Result<Self> {
        match transfer {
            TransferCharacteristic::Bt709
            | TransferCharacteristic::Bt601
            | TransferCharacteristic::Bt2020 => Ok(Self::Bt1886),
            TransferCharacteristic::Srgb => Ok(Self::Srgb),
            TransferCharacteristic::Pq => Ok(Self::Pq),
            TransferCharacteristic::Hlg => Ok(Self::Hlg),
            other => Err(Error::UnsupportedTransfer(other)),
        }
    }

    /// The numeric id as an `f32` (tile uniform) — exact for `0..=3`.
    #[must_use]
    pub fn as_f32(self) -> f32 {
        match self {
            Self::Bt1886 => 0.0,
            Self::Srgb => 1.0,
            Self::Pq => 2.0,
            Self::Hlg => 3.0,
        }
    }

    /// The numeric id as a `u32` (encode uniform).
    #[must_use]
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Bt1886 => 0,
            Self::Srgb => 1,
            Self::Pq => 2,
            Self::Hlg => 3,
        }
    }
}

/// Per-tile uniform block. Mirrors `TileParams` in `composite.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct TileParams {
    /// `[dst_x, dst_y, src_w, src_h]` in pixels.
    pub placement: [u32; 4],
    /// `[dst_w, dst_h, 0, 0]` in pixels — the destination rect the source is
    /// scaled into (scale-at-composite, RT-6 / ADR-0034). Equal to `src_w`/`src_h`
    /// for a 1:1 tile (no scaling).
    pub dst_size: [u32; 4],
    /// `[opacity, transfer_id, 0, 0]`.
    pub opacity_transfer: [f32; 4],
    /// `[luma_scale, luma_off, chroma_scale, 0]`.
    pub range: [f32; 4],
    /// YUV'->R'G'B' row 0 (`[m00, m01, m02, 0]`).
    pub yuv2rgb0: [f32; 4],
    /// YUV'->R'G'B' row 1.
    pub yuv2rgb1: [f32; 4],
    /// YUV'->R'G'B' row 2.
    pub yuv2rgb2: [f32; 4],
    /// Primaries source->canvas row 0.
    pub prim0: [f32; 4],
    /// Primaries source->canvas row 1.
    pub prim1: [f32; 4],
    /// Primaries source->canvas row 2.
    pub prim2: [f32; 4],
}

/// Composite-pass uniform block. Mirrors `CompositeUniforms` in `composite.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct CompositeUniforms {
    /// `[canvas_w, canvas_h, tile_count, 0]`.
    pub canvas: [u32; 4],
    /// Straight-alpha linear RGBA background in the canvas gamut.
    pub background: [f32; 4],
}

/// Encode-pass uniform block. Mirrors `EncodeUniforms` in `encode.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct EncodeUniforms {
    /// `[canvas_w, canvas_h, transfer_id, 0]`.
    pub canvas: [u32; 4],
    /// Canvas RGB->YUV row 0.
    pub rgb2yuv0: [f32; 4],
    /// Canvas RGB->YUV row 1.
    pub rgb2yuv1: [f32; 4],
    /// Canvas RGB->YUV row 2.
    pub rgb2yuv2: [f32; 4],
    /// `[luma_scale, luma_off, chroma_scale, 0]`.
    pub range: [f32; 4],
}

/// Narrow an `f64` matrix coefficient to the `f32` the shader consumes,
/// matching the CPU reference's `demote`.
fn demote(value: f64) -> f32 {
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    // reason: deliberate, documented narrowing of an f64 color-math coefficient
    // to the f32 the shader consumes; identical to crate::matrix::demote so the
    // GPU constants match the CPU oracle. `as` is the only f64->f32 conversion.
    let narrowed = value as f32;
    narrowed
}

/// The YUV'->R'G'B' 3x3 matrix (gamma space) for a matrix-coefficients axis,
/// using the same general derivation as [`crate::matrix::yuv_to_rgb`] so the
/// GPU multiply reproduces the CPU reference exactly (modulo f32 rounding).
///
/// Inputs to the shader are normalized luma `Y in [0,1]` and centered chroma
/// `Cb, Cr in [-0.5, 0.5]`.
///
/// # Errors
///
/// Returns [`Error::UnsupportedMatrix`] for `Unspecified`/`Rgb` (no luma
/// weights), mirroring [`crate::matrix::yuv_to_rgb`].
fn yuv_to_rgb_matrix(coeffs: MatrixCoefficients) -> Result<[[f32; 4]; 3]> {
    let (kr, kg, kb) = matrix::luma_weights(coeffs).ok_or(Error::UnsupportedMatrix(coeffs))?;
    // R' = Y + 2(1-Kr) Cr
    // G' = Y - (2 Kr (1-Kr)/Kg) Cr - (2 Kb (1-Kb)/Kg) Cb
    // B' = Y + 2(1-Kb) Cb
    let row_r = [1.0, 0.0, 2.0 * (1.0 - kr)];
    let row_g = [
        1.0,
        -(2.0 * kb * (1.0 - kb) / kg),
        -(2.0 * kr * (1.0 - kr) / kg),
    ];
    let row_b = [1.0, 2.0 * (1.0 - kb), 0.0];
    Ok([
        [demote(row_r[0]), demote(row_r[1]), demote(row_r[2]), 0.0],
        [demote(row_g[0]), demote(row_g[1]), demote(row_g[2]), 0.0],
        [demote(row_b[0]), demote(row_b[1]), demote(row_b[2]), 0.0],
    ])
}

/// The R'G'B'->YUV' 3x3 matrix (gamma space) for the canvas matrix axis, the
/// inverse pair of [`yuv_to_rgb_matrix`] (mirrors [`crate::matrix::rgb_to_yuv`]).
///
/// # Errors
///
/// Returns [`Error::UnsupportedMatrix`] for `Unspecified`/`Rgb`.
fn rgb_to_yuv_matrix(coeffs: MatrixCoefficients) -> Result<[[f32; 4]; 3]> {
    let (kr, kg, kb) = matrix::luma_weights(coeffs).ok_or(Error::UnsupportedMatrix(coeffs))?;
    // Y  = Kr R' + Kg G' + Kb B'
    // Cb = (B' - Y) / (2(1-Kb))
    // Cr = (R' - Y) / (2(1-Kr))
    let luma = [kr, kg, kb];
    let blue_scale = 1.0 / (2.0 * (1.0 - kb));
    let blue_diff = [-kr * blue_scale, -kg * blue_scale, (1.0 - kb) * blue_scale];
    let red_scale = 1.0 / (2.0 * (1.0 - kr));
    let red_diff = [(1.0 - kr) * red_scale, -kg * red_scale, -kb * red_scale];
    Ok([
        [demote(luma[0]), demote(luma[1]), demote(luma[2]), 0.0],
        [
            demote(blue_diff[0]),
            demote(blue_diff[1]),
            demote(blue_diff[2]),
            0.0,
        ],
        [
            demote(red_diff[0]),
            demote(red_diff[1]),
            demote(red_diff[2]),
            0.0,
        ],
    ])
}

/// Range-expand scale/offset matching [`crate::range::expand_luma`] /
/// [`crate::range::expand_chroma`]: `Y = (Y8 - luma_off)/luma_scale`,
/// `C = (C8 - 128)/chroma_scale`.
fn expand_range_params(r: ColorRange) -> [f32; 4] {
    match r {
        ColorRange::Full => [255.0, 0.0, 255.0, 0.0],
        // Limited / Unspecified / future -> limited path (same default as the
        // CPU reference, which treats everything non-Full as limited).
        _ => [219.0, 16.0, 224.0, 0.0],
    }
}

/// Range-compress scale/offset matching [`crate::range::compress_luma`] /
/// [`crate::range::compress_chroma`]: `Y8 = Y*luma_scale + luma_off`,
/// `C8 = C*chroma_scale + 128`.
fn compress_range_params(r: ColorRange) -> [f32; 4] {
    match r {
        ColorRange::Full => [255.0, 0.0, 255.0, 0.0],
        _ => [219.0, 16.0, 224.0, 0.0],
    }
}

/// The primaries source->canvas conversion matrix in linear light, from
/// [`crate::primaries::convert_matrix`] (identity when gamuts match).
///
/// # Errors
///
/// Returns [`Error::UnsupportedPrimaries`] when either gamut is undefined.
fn primaries_matrix(source: ColorPrimaries, canvas: ColorPrimaries) -> Result<[[f32; 4]; 3]> {
    let m = primaries::convert_matrix(source, canvas)?;
    Ok([
        [demote(m[0][0]), demote(m[0][1]), demote(m[0][2]), 0.0],
        [demote(m[1][0]), demote(m[1][1]), demote(m[1][2]), 0.0],
        [demote(m[2][0]), demote(m[2][1]), demote(m[2][2]), 0.0],
    ])
}

impl TileParams {
    /// Build the per-tile uniform from a placed tile's geometry, opacity, and
    /// resolved [`ColorInfo`], targeting the given canvas.
    ///
    /// The tile color must be fully resolved (run
    /// [`multiview_core::color::ColorInfo::resolve_defaults`] first).
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnresolvedColor`] for an unspecified axis, or the
    /// `Unsupported*` variants when an axis has no shader implementation —
    /// exactly the errors the CPU reference raises.
    #[allow(clippy::too_many_arguments)]
    // reason: a tile uniform is the placement (dst origin + dst size + src size) +
    // opacity + the resolved colour axes; grouping them into a struct here would
    // just shift the same fields and obscure the 1:1-to-`Tile` mapping.
    pub fn build(
        dst_x: u32,
        dst_y: u32,
        dst_w: u32,
        dst_h: u32,
        src_w: u32,
        src_h: u32,
        opacity: f32,
        tile: ColorInfo,
        canvas: CanvasColor,
    ) -> Result<Self> {
        let tile_range = range::require_resolved(tile.range)?;
        if tile.transfer == TransferCharacteristic::Unspecified {
            return Err(Error::UnresolvedColor("transfer"));
        }
        if tile.matrix == MatrixCoefficients::Unspecified {
            return Err(Error::UnresolvedColor("matrix"));
        }
        if tile.primaries == ColorPrimaries::Unspecified {
            return Err(Error::UnresolvedColor("primaries"));
        }
        let yuv = yuv_to_rgb_matrix(tile.matrix)?;
        let prim = primaries_matrix(tile.primaries, canvas.primaries)?;
        let transfer = TransferId::from_transfer(tile.transfer)?;
        // A zero destination size means "place 1:1" — fall back to the source size.
        let dw = if dst_w == 0 { src_w } else { dst_w };
        let dh = if dst_h == 0 { src_h } else { dst_h };
        Ok(Self {
            placement: [dst_x, dst_y, src_w, src_h],
            dst_size: [dw, dh, 0, 0],
            opacity_transfer: [opacity.clamp(0.0, 1.0), transfer.as_f32(), 0.0, 0.0],
            range: expand_range_params(tile_range),
            yuv2rgb0: yuv[0],
            yuv2rgb1: yuv[1],
            yuv2rgb2: yuv[2],
            prim0: prim[0],
            prim1: prim[1],
            prim2: prim[2],
        })
    }
}

impl EncodeUniforms {
    /// Build the encode-pass uniform for a canvas of `width x height`.
    ///
    /// # Errors
    ///
    /// Returns an `Unsupported*` variant if a canvas axis has no shader
    /// implementation.
    pub fn build(width: u32, height: u32, canvas: CanvasColor) -> Result<Self> {
        let rgb2yuv = rgb_to_yuv_matrix(canvas.matrix)?;
        let transfer = TransferId::from_transfer(canvas.transfer)?;
        Ok(Self {
            canvas: [width, height, transfer.as_u32(), 0],
            rgb2yuv0: rgb2yuv[0],
            rgb2yuv1: rgb2yuv[1],
            rgb2yuv2: rgb2yuv[2],
            range: compress_range_params(canvas.range),
        })
    }
}
