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
    /// `[dst_w, dst_h, has_alpha, alpha_layer]` — the destination rect (pixels)
    /// the source is scaled into (scale-at-composite, RT-6 / ADR-0034; equal to
    /// `src_w`/`src_h` for a 1:1 tile), then the NV12+A flag (`1` = sample the
    /// straight R8 alpha array, `0` = opaque) and the tile's layer in that array
    /// (ADR-0058: the spare lanes carry alpha, no struct growth).
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

/// `KeyParams::head[3]` bits 0-1: the key kind (`0` = no key).
pub const KEY_KIND_BITS: u32 = 0b11;
/// Key kind: linear (matte 1 with no key source, else the key's clamped Y).
pub const KEY_KIND_LINEAR: u32 = 1;
/// Key kind: luma (`clamp((y − clip) × gain, 0, 1)`).
pub const KEY_KIND_LUMA: u32 = 2;
/// Invert the matte (a matte from a signal only; ignored by a linear self
/// key). Already folded into the matte table; kept for inspection.
pub const KEY_INVERT: u32 = 1 << 2;
/// The key signal is a separate source resident at `head[0]` of `y_planes`.
pub const KEY_HAS_SOURCE: u32 = 1 << 3;
/// A source-space mask in `KeyParams::mask` applies.
pub const KEY_HAS_MASK: u32 = 1 << 4;
/// The matte comes from a signal: it is `mattes[table[0] + code]`, the
/// 256-entry table of the signal's 8-bit Y code (review MEDIA-1).
pub const KEY_HAS_TABLE: u32 = 1 << 5;
/// Shift of the 2-bit scene group (`0` top, `1` both, `2` out, `3` in).
pub const KEY_GROUP_SHIFT: u32 = 8;
/// Entries of one matte table: one per 8-bit Y code.
pub const MATTE_TABLE_LEN: usize = 256;

/// Per-tile key and scene-group block (ADR-0113, design note §3.3), one entry
/// per tile, parallel to `tiles`. Mirrors `KeyParams` in `composite.wgsl`.
/// All zero is an ordinary `Top` tile with no key.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct KeyParams {
    /// `[key_layer, key_w, key_h, flags]`: the key source's layer in the tile
    /// Y array and its size (zero without a source), then the `KEY_*` bits.
    pub head: [u32; 4],
    /// `[base, 0, 0, 0]`: the first entry of this tile's matte table in the
    /// `mattes` storage buffer (binding 7), valid under [`KEY_HAS_TABLE`].
    pub table: [u32; 4],
    /// `[x, y, w, h]` of the mask in fill-source pixels.
    pub mask: [u32; 4],
    /// `[kind, p | d, i, 0]` of the segment merge (`0` MIX, `1` DIP), weights
    /// already clamped to `[0, 1]` with NaN read as 0.
    pub merge: [f32; 4],
    /// The opaque DIP colour (linear, canvas gamut), alpha 1.
    pub dip_colour: [f32; 4],
}

impl KeyParams {
    /// Build the key/group block of `tile`. `key_layer` is the `y_planes`
    /// layer its key source was uploaded to (`None` without a key source).
    /// A matte from a signal (luma, or linear with a key source) appends its
    /// 256-entry table to `mattes` (see [`push_matte_table`]) and records
    /// its base.
    ///
    /// # Errors
    ///
    /// [`Error::UnresolvedColor`] when the signal's range is unresolved
    /// (exactly what the CPU oracle raises); [`Error::GpuLimit`] when the
    /// table base overflows `u32`.
    pub fn build(
        tile: &crate::pipeline::Tile<'_>,
        key_layer: Option<u32>,
        mattes: &mut Vec<f32>,
    ) -> Result<Self> {
        use crate::key::{unit, KeyKind, SceneMember, SceneMerge, TileGroup};
        let mut out = Self::zeroed();
        if let Some(key) = &tile.key {
            let mut flags = match key.kind {
                KeyKind::Linear => KEY_KIND_LINEAR,
                KeyKind::Luma { .. } => KEY_KIND_LUMA,
            };
            if key.invert {
                flags |= KEY_INVERT;
            }
            if let Some(mask) = key.mask {
                flags |= KEY_HAS_MASK;
                out.mask = [mask.x, mask.y, mask.w, mask.h];
            }
            let source = key.source.zip(key_layer);
            if let Some((src, layer)) = source {
                out.head[0] = layer;
                out.head[1] = src.width();
                out.head[2] = src.height();
                flags |= KEY_HAS_SOURCE;
            }
            // The signal: the key source, else the fill (a luma self key). A
            // linear self key has none (matte 1, invert ignored).
            let signal = match (source, key.kind) {
                (Some((src, _)), _) => Some(src),
                (None, KeyKind::Luma { .. }) => Some(tile.image),
                (None, _) => None,
            };
            if let Some(img) = signal {
                let r = range::require_resolved(img.color().range)?;
                let base = u32::try_from(mattes.len())
                    .map_err(|_| Error::GpuLimit("matte table base overflows u32".to_owned()))?;
                push_matte_table(mattes, key.kind, r, key.invert);
                out.table[0] = base;
                flags |= KEY_HAS_TABLE;
            }
            out.head[3] = flags;
        }
        if let TileGroup::Scene { member, merge } = tile.group {
            let g: u32 = match member {
                SceneMember::Both => 1,
                SceneMember::Out => 2,
                SceneMember::In => 3,
            };
            out.head[3] |= g << KEY_GROUP_SHIFT;
            match merge {
                SceneMerge::Mix { p } => out.merge = [0.0, unit(p), 0.0, 0.0],
                SceneMerge::Dip { colour, d, i } => {
                    out.merge = [1.0, unit(d), unit(i), 0.0];
                    out.dip_colour = [colour.r, colour.g, colour.b, 1.0];
                }
            }
        }
        Ok(out)
    }

    /// The `y_planes` layer of the key source, or `None`.
    #[must_use]
    pub const fn key_layer(&self) -> Option<u32> {
        if self.head[3] & KEY_HAS_SOURCE == 0 {
            None
        } else {
            Some(self.head[0])
        }
    }
}

/// Append the 256-entry matte table of a signal-based key to `out`: entry `c`
/// is the final matte (after invert) for 8-bit Y code `c` of a signal in
/// `range`, computed with the CPU oracle's own functions
/// ([`range::expand_luma`], then [`crate::key::luma_matte`] or
/// [`crate::key::unit`]). The matte is a pure function of the code, so the
/// shader's lookup is exact by construction: no step of a huge or infinite
/// gain can flip on float fuzz (review MEDIA-1). Only `extend`s, so a pooled
/// `out` reuses its capacity.
pub fn push_matte_table(
    out: &mut Vec<f32>,
    kind: crate::key::KeyKind,
    range: multiview_core::color::ColorRange,
    invert: bool,
) {
    use crate::key::{luma_matte, unit, KeyKind};
    out.extend((0..=u8::MAX).map(|c| {
        let y = range::expand_luma(c, range);
        let m = match kind {
            KeyKind::Luma { clip, gain } => luma_matte(y, clip, gain),
            KeyKind::Linear => unit(y),
        };
        if invert {
            1.0 - m
        } else {
            m
        }
    }));
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
            // `unit` reads a NaN opacity (a keyer weight rides it) as 0, as the
            // CPU oracle does; a NaN must never reach the shader's clamp.
            opacity_transfer: [crate::key::unit(opacity), transfer.as_f32(), 0.0, 0.0],
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

impl TileParams {
    /// Mark this tile as carrying a straight per-pixel alpha plane resident in
    /// `layer` of the composite pass's R8 alpha texture array. The shader then
    /// multiplies that alpha into the uniform opacity at the single premultiply
    /// step (ADR-0058; inv #8 order unchanged).
    #[must_use]
    pub const fn with_alpha_layer(mut self, layer: u32) -> Self {
        self.dst_size[2] = 1;
        self.dst_size[3] = layer;
        self
    }

    /// Carry a wipe mask (ADR-0112 §3) in the spare `.z/.w` lanes, the same
    /// no-growth precedent as the alpha layer: `opacity_transfer.z` = shape
    /// id (`0` = none), `.w` = reverse; `range.w` = softness; `yuv2rgb0..2.w`
    /// = origin x, origin y, extent; `prim0.w` = radius, `prim1.w` = expand.
    /// The shader derives the aspect from the canvas size, as the CPU does.
    #[must_use]
    pub fn with_mask(mut self, mask: &crate::mask::WipeMask, canvas_w: u32, canvas_h: u32) -> Self {
        // Exact endpoints (review MED-01): coverage 0 is zero opacity,
        // coverage 1 is no mask at all; neither depends on float rounding.
        // A malformed public mask draws nothing, exactly as on the CPU
        // reference (`WipeMask::is_valid_for`; the shader evaluates the field
        // on the canvas size, which this pins equal to the mask's own).
        if !mask.is_valid_for(canvas_w, canvas_h) {
            self.opacity_transfer[0] = 0.0;
            return self;
        }
        match mask.pinned {
            Some(c) if c <= 0.0 => {
                self.opacity_transfer[0] = 0.0;
                return self;
            }
            Some(_) => return self,
            None => {}
        }
        self.opacity_transfer[2] = shape_lane(mask.shape.id());
        self.opacity_transfer[3] = if mask.reverse { 1.0 } else { 0.0 };
        self.range[3] = mask.softness;
        self.yuv2rgb0[3] = mask.origin[0];
        self.yuv2rgb1[3] = mask.origin[1];
        self.yuv2rgb2[3] = mask.extent;
        self.prim0[3] = mask.radius;
        self.prim1[3] = mask.expand;
        self
    }

    /// The alpha-array layer this tile samples, or `None` for an opaque tile.
    #[must_use]
    pub const fn alpha_layer(&self) -> Option<u32> {
        if self.dst_size[2] == 0 {
            None
        } else {
            Some(self.dst_size[3])
        }
    }
}

/// A small shape id as the `f32` lane the shader reads (exact for `0..=5`).
fn shape_lane(id: u32) -> f32 {
    match id {
        1 => 1.0,
        2 => 2.0,
        3 => 3.0,
        4 => 4.0,
        5 => 5.0,
        _ => 0.0,
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

#[cfg(test)]
mod key_params_tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use multiview_core::color::{
        ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
    };

    use super::*;
    use crate::key::{KeyMask, SceneMember, SceneMerge, TileGroup, TileKey};
    use crate::pipeline::{Nv12Image, Tile};

    fn img(w: u32, h: u32, range: ColorRange) -> Nv12Image {
        let color = ColorInfo {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristic::Bt709,
            matrix: MatrixCoefficients::Bt709,
            range,
        };
        Nv12Image::solid(w, h, 100, 128, 128, color).unwrap()
    }

    #[test]
    fn an_ordinary_tile_packs_all_zero() {
        let a = img(4, 4, ColorRange::Limited);
        let mut mattes = Vec::new();
        let k = KeyParams::build(&Tile::placed(&a, 0, 0, 1.0), None, &mut mattes).unwrap();
        assert_eq!(k, KeyParams::zeroed());
        assert_eq!(mattes.len(), 0);
        assert_eq!(core::mem::size_of::<KeyParams>(), 80);
        // The linear self key has no signal, hence no table.
        let t = Tile::placed(&a, 0, 0, 1.0).with_key(TileKey::linear().inverted(true));
        let k = KeyParams::build(&t, None, &mut mattes).unwrap();
        assert_eq!(k.head[3] & KEY_HAS_TABLE, 0);
        assert_eq!(mattes.len(), 0);
    }

    #[test]
    fn a_key_source_carries_its_own_size_layer_and_table() {
        let fill = img(4, 4, ColorRange::Limited);
        let key = img(8, 6, ColorRange::Full);
        let t = Tile::placed(&fill, 0, 0, 1.0).with_key(
            TileKey::luma(0.25, 2.0)
                .with_source(&key)
                .inverted(true)
                .masked(KeyMask {
                    x: 1,
                    y: 2,
                    w: 3,
                    h: u32::MAX,
                }),
        );
        let mut mattes = vec![0.5; 7];
        let k = KeyParams::build(&t, Some(9), &mut mattes).unwrap();
        assert_eq!(k.key_layer(), Some(9));
        assert_eq!(&k.head[..3], &[9, 8, 6]);
        let flags = k.head[3];
        assert_eq!(flags & KEY_KIND_BITS, KEY_KIND_LUMA);
        assert_ne!(flags & KEY_INVERT, 0);
        assert_ne!(flags & KEY_HAS_MASK, 0);
        assert_ne!(flags & KEY_HAS_TABLE, 0);
        assert_eq!(k.table, [7, 0, 0, 0]);
        assert_eq!(mattes.len(), 7 + MATTE_TABLE_LEN);
        // Built with the KEY SOURCE's (full) range, inverted.
        for c in 0..=u8::MAX {
            let y = range::expand_luma(c, ColorRange::Full);
            let want = 1.0 - crate::key::luma_matte(y, 0.25, 2.0);
            assert_eq!(mattes[7 + usize::from(c)], want, "code {c}");
        }
        assert_eq!(k.mask, [1, 2, 3, u32::MAX]);
    }

    #[test]
    fn the_matte_table_is_the_oracle_matte_for_every_code() {
        let nan = f32::NAN;
        let inf = f32::INFINITY;
        let sub = -1e-40_f32;
        assert!(sub.is_subnormal());
        let clips = [0.2, 0.0, nan, inf, -inf, sub, -sub, 0.502_283_1];
        let gains = [2.0, 1e30, inf, -inf, -1.0, 0.0, nan];
        for range in [ColorRange::Limited, ColorRange::Full] {
            for &clip in &clips {
                for &gain in &gains {
                    for invert in [false, true] {
                        let fill = img(4, 4, range);
                        let t = Tile::placed(&fill, 0, 0, 1.0)
                            .with_key(TileKey::luma(clip, gain).inverted(invert));
                        let mut mattes = Vec::new();
                        let k = KeyParams::build(&t, None, &mut mattes).unwrap();
                        assert_eq!(k.table[0], 0);
                        assert_eq!(mattes.len(), MATTE_TABLE_LEN);
                        for c in 0..=u8::MAX {
                            let y = range::expand_luma(c, range);
                            let m = crate::key::luma_matte(y, clip, gain);
                            let want = if invert { 1.0 - m } else { m };
                            let got = mattes[usize::from(c)];
                            assert!(
                                got.to_bits() == want.to_bits() && (0.0..=1.0).contains(&got),
                                "{range:?} clip={clip} gain={gain} invert={invert} code={c}: \
                                 {got} != {want}"
                            );
                        }
                    }
                }
            }
        }
        // Review MEDIA-1: a full-range black code, a negative subnormal clip
        // and an infinite gain is matte 1 (a positive excess times +inf).
        let mut mattes = Vec::new();
        push_matte_table(
            &mut mattes,
            crate::key::KeyKind::Luma {
                clip: sub,
                gain: inf,
            },
            ColorRange::Full,
            false,
        );
        assert_eq!(mattes[0], 1.0);
        // Linear with a key source: unit(y) of the source's range.
        let fill = img(4, 4, ColorRange::Full);
        let key = img(4, 4, ColorRange::Limited);
        let t = Tile::placed(&fill, 0, 0, 1.0).with_key(TileKey::linear().with_source(&key));
        let mut mattes = Vec::new();
        KeyParams::build(&t, Some(1), &mut mattes).unwrap();
        assert_eq!(mattes[16], 0.0);
        assert_eq!(mattes[235], 1.0);
        assert_eq!(mattes[255], 1.0);
        assert_eq!(mattes[0], 0.0);
    }

    #[test]
    fn scene_merge_weights_are_clamped_and_nan_safe() {
        let a = img(4, 4, ColorRange::Limited);
        let g = |member, merge| {
            KeyParams::build(
                &Tile::placed(&a, 0, 0, 1.0).in_group(TileGroup::Scene { member, merge }),
                None,
                &mut Vec::new(),
            )
            .unwrap()
        };
        let k = g(SceneMember::Out, SceneMerge::Mix { p: f32::NAN });
        assert_eq!(k.head[3] >> KEY_GROUP_SHIFT, 2);
        assert_eq!(k.merge, [0.0, 0.0, 0.0, 0.0]);
        let k = g(
            SceneMember::In,
            SceneMerge::Dip {
                colour: crate::blend::LinearRgba::opaque(0.1, 0.2, 0.3),
                d: 2.0,
                i: -1.0,
            },
        );
        assert_eq!(k.head[3] >> KEY_GROUP_SHIFT, 3);
        assert_eq!(k.merge, [1.0, 1.0, 0.0, 0.0]);
        assert_eq!(k.dip_colour, [0.1, 0.2, 0.3, 1.0]);
        let k = g(SceneMember::Both, SceneMerge::Mix { p: 0.4 });
        assert_eq!(k.head[3] >> KEY_GROUP_SHIFT, 1);
        // A NaN tile opacity (a keyer weight) reads as 0, never a NaN lane.
        let p = TileParams::build(
            0,
            0,
            4,
            4,
            4,
            4,
            f32::NAN,
            a.color(),
            crate::pipeline::CanvasColor::default(),
        )
        .unwrap();
        assert_eq!(p.opacity_transfer[0], 0.0);
    }
}
