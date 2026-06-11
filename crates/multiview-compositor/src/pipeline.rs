//! The fixed-order color pipeline (invariant #8) and a small CPU reference
//! compositor over NV12.
//!
//! This module wires the pure color-math modules into the **one canonical
//! order** (color-management.md §2), which must never be reordered:
//!
//! detect 4 axes -> range expand -> YUV->RGB matrix -> linearize (EOTF) ->
//! primaries convert (in linear) -> scale + premultiplied-alpha blend (in
//! linear) -> OETF -> RGB->YUV + range compress -> tag output.
//!
//! The compositor here is a **CPU reference** (golden-frame, bit-exact on CPU);
//! the GPU/wgpu path is out of scope for this module and feature-gated off by
//! default. The canvas is fixed SDR BT.709 limited (ADR-C001); the working
//! blend space is linear BT.709-primaries RGB.

use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

use crate::blend::{over, LinearRgba, PremulRgba};
use crate::error::{Error, Result};
use crate::transfer_lut::LutSet;
use crate::{matrix, primaries, range, transfer};

/// The fixed canvas color description (ADR-C001).
///
/// Default is SDR BT.709 limited: BT.709 primaries, BT.709/BT.1886 transfer,
/// BT.709-NCL matrix, limited range. The working blend buffer is linear in the
/// canvas **primaries**; the canvas transfer is applied (OETF) only on encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasColor {
    /// Canvas primaries (gamut) — the linear blend space.
    pub primaries: ColorPrimaries,
    /// Canvas transfer (applied on the encode-side OETF).
    pub transfer: TransferCharacteristic,
    /// Canvas YUV<->RGB matrix (applied on the encode-side RGB->YUV).
    pub matrix: MatrixCoefficients,
    /// Canvas quantization range (applied on the encode-side compression).
    pub range: ColorRange,
}

impl Default for CanvasColor {
    /// The default SDR BT.709 limited canvas (ADR-C001).
    fn default() -> Self {
        Self {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristic::Bt709,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
        }
    }
}

impl CanvasColor {
    /// The [`ColorInfo`] this canvas tags its output with (the "tag output"
    /// step). Tagging only labels; the pixels were produced to match.
    #[must_use]
    pub const fn output_tag(self) -> ColorInfo {
        ColorInfo {
            primaries: self.primaries,
            transfer: self.transfer,
            matrix: self.matrix,
            range: self.range,
        }
    }
}

/// Convert one tile YUV sample (8-bit code values) through the **front half**
/// of the fixed pipeline into linear RGB in the **canvas gamut**, ready to
/// blend.
///
/// Order, exactly: range-expand -> YUV->RGB matrix (gamma R'G'B') ->
/// linearize (tile EOTF) -> primaries convert into the canvas gamut (linear).
///
/// `tile_color` must be fully resolved (no `Unspecified` axis); run
/// [`ColorInfo::resolve_defaults`] first.
///
/// # Errors
///
/// Returns [`Error::UnresolvedColor`] if any axis is unspecified, or the
/// `Unsupported*` variants when an axis has no CPU-reference implementation.
pub fn tile_yuv_to_canvas_linear(
    y8: u8,
    cb8: u8,
    cr8: u8,
    tile_color: ColorInfo,
    canvas: CanvasColor,
) -> Result<[f32; 3]> {
    // detect/resolve guard: the kernel never sees Unspecified.
    let tile_range = range::require_resolved(tile_color.range)?;
    if tile_color.transfer == TransferCharacteristic::Unspecified {
        return Err(Error::UnresolvedColor("transfer"));
    }
    if tile_color.matrix == MatrixCoefficients::Unspecified {
        return Err(Error::UnresolvedColor("matrix"));
    }
    if tile_color.primaries == ColorPrimaries::Unspecified {
        return Err(Error::UnresolvedColor("primaries"));
    }

    // 1. range expand (code-value space).
    let y = range::expand_luma(y8, tile_range);
    let cb = range::expand_chroma(cb8, tile_range);
    let cr = range::expand_chroma(cr8, tile_range);

    // 2. YUV' -> R'G'B' (gamma-encoded).
    let rgb_gamma = matrix::yuv_to_rgb(y, cb, cr, tile_color.matrix)?;

    // 3. linearize via the tile's own EOTF.
    let lin = [
        transfer::eotf(rgb_gamma[0], tile_color.transfer)?,
        transfer::eotf(rgb_gamma[1], tile_color.transfer)?,
        transfer::eotf(rgb_gamma[2], tile_color.transfer)?,
    ];

    // 4. primaries convert into the canvas gamut (linear light).
    let conv = primaries::convert_matrix(tile_color.primaries, canvas.primaries)?;
    Ok(primaries::apply(conv, lin))
}

/// Convert a linear RGB triple in the **canvas gamut** through the **back half**
/// of the fixed pipeline into output YUV 8-bit code values.
///
/// Order, exactly: canvas OETF -> RGB->YUV (canvas matrix) -> range compress.
///
/// # Errors
///
/// Returns an `Unsupported*` variant if a canvas axis has no CPU-reference
/// implementation.
pub fn canvas_linear_to_output_yuv(lin: [f32; 3], canvas: CanvasColor) -> Result<[u8; 3]> {
    // 6. canvas OETF (linear -> gamma code values).
    let gamma = [
        transfer::oetf(lin[0], canvas.transfer)?,
        transfer::oetf(lin[1], canvas.transfer)?,
        transfer::oetf(lin[2], canvas.transfer)?,
    ];
    // 7. RGB -> YUV with the canvas matrix.
    let yuv = matrix::rgb_to_yuv(gamma[0], gamma[1], gamma[2], canvas.matrix)?;
    // 8. range compress to 8-bit code values.
    Ok([
        range::compress_luma(yuv[0], canvas.range),
        range::compress_chroma(yuv[1], canvas.range),
        range::compress_chroma(yuv[2], canvas.range),
    ])
}

/// Convert one 8-bit sRGB-ish RGB sample to the canvas's output YUV code values.
///
/// Runs the canonical back half of the fixed colour pipeline (inv #8): per-channel
/// EOTF to linear light, then [`canvas_linear_to_output_yuv`]. Used by the
/// synthetic `solid`/`bars` source constructors so their colours land in exactly
/// the canvas output space a decoded source's samples are converted into.
fn rgb_to_canvas_yuv(r: u8, g: u8, b: u8, canvas: CanvasColor) -> Result<[u8; 3]> {
    let lin = [
        transfer::eotf(f32::from(r) / 255.0, canvas.transfer)?,
        transfer::eotf(f32::from(g) / 255.0, canvas.transfer)?,
        transfer::eotf(f32::from(b) / 255.0, canvas.transfer)?,
    ];
    canvas_linear_to_output_yuv(lin, canvas)
}

/// A tiny owned NV12 image: a `width x height` Y plane followed by a
/// `width x (height/2)` interleaved Cb/Cr plane (4:2:0 semi-planar).
///
/// This is the CPU-reference pixel container — the GPU path uses native
/// surfaces and never materializes one of these. Width and height are required
/// to be even (4:2:0 chroma subsampling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Image {
    width: u32,
    height: u32,
    y_plane: Vec<u8>,
    uv_plane: Vec<u8>,
    /// The resolved color of these samples.
    color: ColorInfo,
}

impl Nv12Image {
    /// Build an NV12 image from explicit planes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Geometry`] if `width`/`height` are not both even and
    /// positive, or if a plane length does not match the geometry
    /// (`y = width*height`, `uv = width*height/2`).
    pub fn new(
        width: u32,
        height: u32,
        y_plane: Vec<u8>,
        uv_plane: Vec<u8>,
        color: ColorInfo,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::Geometry(format!(
                "NV12 dimensions must be positive and even (got {width}x{height})"
            )));
        }
        let pixels = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .ok_or_else(|| Error::Geometry("dimensions overflow usize".to_owned()))?;
        let y_len = pixels.0 * pixels.1;
        let uv_len = y_len / 2;
        if y_plane.len() != y_len {
            return Err(Error::Geometry(format!(
                "Y plane length {} != expected {y_len}",
                y_plane.len()
            )));
        }
        if uv_plane.len() != uv_len {
            return Err(Error::Geometry(format!(
                "UV plane length {} != expected {uv_len}",
                uv_plane.len()
            )));
        }
        Ok(Self {
            width,
            height,
            y_plane,
            uv_plane,
            color,
        })
    }

    /// A solid-color NV12 image filled with the given 8-bit `(y, cb, cr)` code
    /// values; useful for tests and placeholder cards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Geometry`] if `width`/`height` are not both even and
    /// positive.
    pub fn solid(width: u32, height: u32, y: u8, cb: u8, cr: u8, color: ColorInfo) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::Geometry(format!(
                "NV12 dimensions must be positive and even (got {width}x{height})"
            )));
        }
        let (w, h) = (
            usize::try_from(width).map_err(|_| Error::Geometry("width overflow".to_owned()))?,
            usize::try_from(height).map_err(|_| Error::Geometry("height overflow".to_owned()))?,
        );
        let y_plane = vec![y; w * h];
        let mut uv_plane = vec![0_u8; w * h / 2];
        for pair in uv_plane.chunks_exact_mut(2) {
            if let [u, v] = pair {
                *u = cb;
                *v = cr;
            }
        }
        Ok(Self {
            width,
            height,
            y_plane,
            uv_plane,
            color,
        })
    }

    /// A solid NV12 source filled with an 8-bit sRGB-ish `(r, g, b)` colour,
    /// converted into the canvas output space (inv #8) and tagged with the
    /// canvas's output [`ColorInfo`]. The synthetic `solid` source kind (ADR-0027).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Geometry`] for non-positive/odd dimensions, or an
    /// `Unsupported*` colour variant if a canvas axis has no CPU implementation.
    pub fn solid_rgb(
        width: u32,
        height: u32,
        r: u8,
        g: u8,
        b: u8,
        canvas: CanvasColor,
    ) -> Result<Self> {
        let [y, cb, cr] = rgb_to_canvas_yuv(r, g, b, canvas)?;
        Self::solid(width, height, y, cb, cr, canvas.output_tag())
    }

    /// 75 % colour bars (7 equal vertical bands, left→right: white, yellow, cyan,
    /// green, magenta, red, blue) in the canvas output space. The synthetic
    /// `bars` source kind (ADR-0027) — the standard line-up signal.
    ///
    /// The classic descending-luma staircase falls out of the colour conversion;
    /// it is the content-aware property the tests assert.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Geometry`] for non-positive/odd dimensions, or an
    /// `Unsupported*` colour variant if a canvas axis has no CPU implementation.
    pub fn color_bars(width: u32, height: u32, canvas: CanvasColor) -> Result<Self> {
        // 75 % amplitude bars (191 = 0.75 * 255).
        const BARS: [(u8, u8, u8); 7] = [
            (191, 191, 191), // white
            (191, 191, 0),   // yellow
            (0, 191, 191),   // cyan
            (0, 191, 0),     // green
            (191, 0, 191),   // magenta
            (191, 0, 0),     // red
            (0, 0, 191),     // blue
        ];
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::Geometry(format!(
                "NV12 dimensions must be positive and even (got {width}x{height})"
            )));
        }
        let yuv = BARS
            .iter()
            .map(|&(r, g, b)| rgb_to_canvas_yuv(r, g, b, canvas))
            .collect::<Result<Vec<[u8; 3]>>>()?;
        let n = yuv.len();
        let (w, h) = (
            usize::try_from(width).map_err(|_| Error::Geometry("width overflow".to_owned()))?,
            usize::try_from(height).map_err(|_| Error::Geometry("height overflow".to_owned()))?,
        );
        // The bar covering output column `x` (left→right), as its YUV triple.
        let bar_of = |x: usize| -> [u8; 3] {
            let idx = (x.saturating_mul(n) / w).min(n.saturating_sub(1));
            yuv.get(idx).copied().unwrap_or([16, 128, 128])
        };
        let mut y_plane = Vec::with_capacity(w.saturating_mul(h));
        for _row in 0..h {
            for x in 0..w {
                let [luma, _, _] = bar_of(x);
                y_plane.push(luma);
            }
        }
        // NV12 chroma: one interleaved Cb,Cr per 2×2 block; sample the block's
        // left column (bars are wide, so the block never straddles two bars at
        // any sane width).
        let mut uv_plane = Vec::with_capacity(w.saturating_mul(h) / 2);
        for _crow in 0..(h / 2) {
            for cx in 0..(w / 2) {
                let [_, cb, cr] = bar_of(cx.saturating_mul(2));
                uv_plane.push(cb);
                uv_plane.push(cr);
            }
        }
        Self::new(width, height, y_plane, uv_plane, canvas.output_tag())
    }

    /// Image width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The resolved color of these samples.
    #[must_use]
    pub const fn color(&self) -> ColorInfo {
        self.color
    }

    /// Borrow the Y plane.
    #[must_use]
    pub fn y_plane(&self) -> &[u8] {
        &self.y_plane
    }

    /// Borrow the interleaved Cb/Cr plane.
    #[must_use]
    pub fn uv_plane(&self) -> &[u8] {
        &self.uv_plane
    }

    /// Sample the `(y, cb, cr)` 8-bit code values at integer pixel `(x, y)`
    /// (nearest, no chroma interpolation — the reference uses simple chroma
    /// replication; correct chroma siting is a GPU concern out of scope here).
    ///
    /// Returns [`None`] if `(x, py)` is outside the image.
    #[must_use]
    pub fn sample(&self, x: u32, py: u32) -> Option<(u8, u8, u8)> {
        if x >= self.width || py >= self.height {
            return None;
        }
        let w = usize::try_from(self.width).ok()?;
        let xi = usize::try_from(x).ok()?;
        let yi = usize::try_from(py).ok()?;
        let y = *self.y_plane.get(yi * w + xi)?;
        // Chroma is subsampled 2x2; the interleaved row stride is `w` bytes
        // (w/2 chroma pairs). Locate the chroma pair for this pixel.
        let cx = xi / 2;
        let cy = yi / 2;
        let uv_index = (cy * w) + (cx * 2);
        let cb = *self.uv_plane.get(uv_index)?;
        let cr = *self.uv_plane.get(uv_index + 1)?;
        Some((y, cb, cr))
    }
}

/// One tile in the reference composite: a source image, the **destination
/// rectangle** it occupies on the canvas, and a uniform opacity.
///
/// The destination rectangle is `[dst_x, dst_x + dst_w) × [dst_y, dst_y + dst_h)`
/// in canvas pixels. When `dst_w`/`dst_h` differ from the source image's own
/// dimensions the compositor **scales-at-composite** (invariant #6: composite at
/// tile/cell resolution), resampling the NV12 source into the destination rect —
/// the cross-geometry router case (RT-6 / ADR-0034): a source decoded at a
/// canonical size composites correctly into a differently-sized cell, never
/// clipped or smeared. When they match, the placement is 1:1 (the prior
/// behaviour, byte-for-byte). The resample stays in NV12 (invariant #5: no
/// per-tile RGBA is materialised) and runs the identical fixed colour pipeline
/// (invariant #8) on each destination pixel's resampled source sample.
///
/// Build with [`Tile::placed`] (1:1) or [`Tile::scaled`] (explicit destination
/// size); the public fields remain accessible for direct construction.
#[derive(Debug, Clone)]
pub struct Tile<'a> {
    /// The source NV12 image (its own resolved color).
    pub image: &'a Nv12Image,
    /// Destination x (pixels) of the tile's top-left on the canvas.
    pub dst_x: u32,
    /// Destination y (pixels) of the tile's top-left on the canvas.
    pub dst_y: u32,
    /// Destination width (pixels) of the tile on the canvas. The source is
    /// scaled to this width when it differs from `image.width()`.
    pub dst_w: u32,
    /// Destination height (pixels) of the tile on the canvas. The source is
    /// scaled to this height when it differs from `image.height()`.
    pub dst_h: u32,
    /// Uniform tile opacity in `[0, 1]` (straight alpha).
    pub opacity: f32,
}

impl<'a> Tile<'a> {
    /// A tile placed **1:1** at `(dst_x, dst_y)` — the destination rectangle is
    /// exactly the source image's own size, so no resampling occurs. This is the
    /// prior placement behaviour, byte-for-byte.
    #[must_use]
    pub const fn placed(image: &'a Nv12Image, dst_x: u32, dst_y: u32, opacity: f32) -> Self {
        Self {
            image,
            dst_x,
            dst_y,
            dst_w: image.width,
            dst_h: image.height,
            opacity,
        }
    }

    /// A tile **scaled** into the destination rectangle
    /// `[dst_x, dst_x + dst_w) × [dst_y, dst_y + dst_h)`. When `dst_w`/`dst_h`
    /// differ from the source dimensions the compositor resamples the NV12 source
    /// into the rect at composite time (scale-at-composite; RT-6 / ADR-0034).
    #[must_use]
    pub const fn scaled(
        image: &'a Nv12Image,
        dst_x: u32,
        dst_y: u32,
        dst_w: u32,
        dst_h: u32,
        opacity: f32,
    ) -> Self {
        Self {
            image,
            dst_x,
            dst_y,
            dst_w,
            dst_h,
            opacity,
        }
    }

    /// The destination width in pixels (the source is scaled to it). Falls back
    /// to the source image width if `dst_w` was left zero (defensive; the
    /// constructors always set it positive).
    #[must_use]
    const fn eff_dst_w(&self) -> u32 {
        if self.dst_w == 0 {
            self.image.width
        } else {
            self.dst_w
        }
    }

    /// The destination height in pixels (the source is scaled to it). Falls back
    /// to the source image height if `dst_h` was left zero (defensive).
    #[must_use]
    const fn eff_dst_h(&self) -> u32 {
        if self.dst_h == 0 {
            self.image.height
        } else {
            self.dst_h
        }
    }

    /// Map a destination-rect pixel offset `(ddx, ddy)` (relative to the tile's
    /// `dst_x`/`dst_y` top-left, both `< dst_w`/`dst_h`) to the source pixel it
    /// samples, using **nearest-neighbour** scaling. When the destination size
    /// equals the source size this is the identity (`ddx, ddy`), so a 1:1 tile is
    /// byte-for-byte the prior behaviour.
    ///
    /// Nearest-neighbour keeps the reference compositor deterministic and panic-
    /// free with no fractional-coordinate chroma siting to special-case; the GPU
    /// fast path can use bilinear/box filtering within its SSIM/PSNR budget.
    fn src_pixel_for(&self, ddx: u32, ddy: u32) -> (u32, u32) {
        let dst_w = self.eff_dst_w();
        let dst_h = self.eff_dst_h();
        let src_w = self.image.width;
        let src_h = self.image.height;
        // `(ddx * src_w) / dst_w`, computed in u64 to avoid overflow, clamped to
        // the last source column/row. No `as` casts (guardrail).
        let sx = map_axis(ddx, src_w, dst_w);
        let sy = map_axis(ddy, src_h, dst_h);
        (sx, sy)
    }
}

/// Nearest-neighbour map of a destination offset `d` in `[0, dst)` to a source
/// index in `[0, src)`: `floor(d * src / dst)`, clamped to `src - 1`. Total and
/// panic-free (u64 intermediate, no `as` cast). Returns `0` for a degenerate
/// `dst == 0` or `src == 0`.
fn map_axis(d: u32, src: u32, dst: u32) -> u32 {
    if dst == 0 || src == 0 {
        return 0;
    }
    if src == dst {
        return d.min(src.saturating_sub(1));
    }
    let num = u64::from(d).saturating_mul(u64::from(src));
    let idx = num / u64::from(dst);
    let clamped = idx.min(u64::from(src.saturating_sub(1)));
    u32::try_from(clamped).unwrap_or(src.saturating_sub(1))
}

/// Composite a back-to-front stack of [`Tile`]s onto a `canvas_w x canvas_h`
/// output, running the full fixed-order pipeline per pixel, and return the
/// tagged output NV12 image.
///
/// Pixels not covered by any tile take `background` (a linear canvas-gamut
/// color). Tiles are drawn in slice order (first = bottom); each tile is placed
/// 1:1 (no scaling — scaling is exercised by the math but a resampler is out of
/// scope for this reference) and clipped to the canvas. Per pixel the front
/// half ([`tile_yuv_to_canvas_linear`]) runs for each covering tile, the
/// premultiplied-alpha [`over`] operator folds them in linear light, then the
/// back half ([`canvas_linear_to_output_yuv`]) encodes the result.
///
/// # Errors
///
/// Returns [`Error::Geometry`] for non-even/zero canvas dimensions, or a color
/// error if a tile/canvas axis is unsupported/unresolved.
pub fn composite(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
) -> Result<Nv12Image> {
    composite_with(canvas_w, canvas_h, canvas, background, tiles, true)
}

/// Composite as [`composite`], but with explicit control over whether the
/// per-pixel EOTF/OETF run through the lookup-table path (`use_lut == true`,
/// ADR-0022) or the un-LUT'd transcendental **oracle** (`use_lut == false`).
///
/// [`composite`] calls this with `use_lut == true`; the oracle path exists so
/// equivalence tests can render the same canvas both ways (the LUT must match
/// the oracle within `±1` code value). The pipeline **order** (invariant #8) is
/// identical on both paths — only the EOTF/OETF *evaluation* changes.
///
/// # Errors
///
/// Same as [`composite`].
pub fn composite_with(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    use_lut: bool,
) -> Result<Nv12Image> {
    let n_threads = auto_thread_count();
    composite_with_threads(
        canvas_w, canvas_h, canvas, background, tiles, use_lut, n_threads,
    )
}

/// The number of worker threads to fan the composite across: the machine's
/// available parallelism, clamped to `[1, 64]`. Falls back to 1 when the count
/// is unavailable (the serial path).
fn auto_thread_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(64)
}

/// Below this many canvas pixels the thread-spawn overhead outweighs the win,
/// so the composite runs serially (also covers tiny placeholder/test canvases).
const PARALLEL_PIXEL_THRESHOLD: usize = 256 * 256;

/// Composite as [`composite_with`], but with an explicit worker-thread count.
///
/// The canvas is partitioned into **even-row-aligned bands** by chroma
/// row-pairs, so each worker owns whole UV rows of a disjoint `&mut` slice — the
/// split is race-free by construction (no shared mutable state on the data
/// plane) and the output is byte-identical regardless of `n_threads`, because
/// every band runs the identical deterministic per-pixel pipeline and rebases
/// the global row for tile addressing. `n_threads <= 1`, or a canvas below
/// [`PARALLEL_PIXEL_THRESHOLD`] pixels, renders serially.
///
/// `n_threads` is clamped to `[1, 64]` and to the number of chroma row-pairs
/// (`canvas_h / 2`) so a band is never empty.
///
/// # Errors
///
/// Same as [`composite_with`]; the first band error (in row order) is returned.
pub fn composite_with_threads(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    use_lut: bool,
    n_threads: usize,
) -> Result<Nv12Image> {
    if canvas_w == 0 || canvas_h == 0 || canvas_w % 2 != 0 || canvas_h % 2 != 0 {
        return Err(Error::Geometry(format!(
            "canvas dimensions must be positive and even (got {canvas_w}x{canvas_h})"
        )));
    }
    let w = usize::try_from(canvas_w)
        .map_err(|_| Error::Geometry("canvas width overflow".to_owned()))?;
    let h = usize::try_from(canvas_h)
        .map_err(|_| Error::Geometry("canvas height overflow".to_owned()))?;

    // Build the transfer LUTs once per composite for exactly the transfers
    // present (tiles + canvas); empty when `use_lut` is false. Unsupported
    // transfers are absent and fall back to the oracle (same Err).
    let luts = if use_lut {
        let mut transfers: Vec<TransferCharacteristic> = vec![canvas.transfer];
        for tile in tiles {
            transfers.push(tile.image.color().transfer);
        }
        Some(LutSet::for_transfers(transfers))
    } else {
        None
    };

    let mut y_plane = vec![0_u8; w * h];
    let mut uv_plane = vec![0_u8; w * h / 2];

    // Total chroma row-pairs; clamp the worker count so no band is empty.
    let total_pairs = h / 2;
    let workers = n_threads.clamp(1, 64).min(total_pairs.max(1));

    if workers <= 1 || w.saturating_mul(h) < PARALLEL_PIXEL_THRESHOLD {
        // Serial path: one band covering the whole canvas.
        composite_band(
            &mut y_plane,
            &mut uv_plane,
            w,
            0,
            canvas_h,
            canvas,
            background,
            tiles,
            luts.as_ref(),
        )?;
    } else {
        composite_parallel(
            &mut y_plane,
            &mut uv_plane,
            w,
            total_pairs,
            workers,
            canvas,
            background,
            tiles,
            luts.as_ref(),
        )?;
    }

    Nv12Image::new(canvas_w, canvas_h, y_plane, uv_plane, canvas.output_tag())
}

/// Render the whole canvas with the **reference** (pixel-driven) kernel
/// ([`composite_band_reference`]) — the byte-exact oracle the tile-driven
/// production kernel ([`composite_band`]) is pinned against.
///
/// This is the original O(pixels × all-tiles) kernel, preserved verbatim so the
/// equivalence proptest can assert the optimized kernel is bit-identical. It is
/// **not** the production path (use [`composite`]); it is exposed only so tests
/// and benches can compare. The result is computed single-threaded so it never
/// depends on the band split being correct.
///
/// `use_lut` selects the LUT (`true`, ADR-0022) or transcendental-oracle
/// (`false`) EOTF/OETF evaluation, exactly as [`composite_with`].
///
/// # Errors
///
/// Same as [`composite_with_threads`].
#[doc(hidden)]
pub fn composite_reference(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    use_lut: bool,
) -> Result<Nv12Image> {
    if canvas_w == 0 || canvas_h == 0 || canvas_w % 2 != 0 || canvas_h % 2 != 0 {
        return Err(Error::Geometry(format!(
            "canvas dimensions must be positive and even (got {canvas_w}x{canvas_h})"
        )));
    }
    let w = usize::try_from(canvas_w)
        .map_err(|_| Error::Geometry("canvas width overflow".to_owned()))?;
    let h = usize::try_from(canvas_h)
        .map_err(|_| Error::Geometry("canvas height overflow".to_owned()))?;

    let luts = if use_lut {
        let mut transfers: Vec<TransferCharacteristic> = vec![canvas.transfer];
        for tile in tiles {
            transfers.push(tile.image.color().transfer);
        }
        Some(LutSet::for_transfers(transfers))
    } else {
        None
    };

    let mut y_plane = vec![0_u8; w * h];
    let mut uv_plane = vec![0_u8; w * h / 2];
    composite_band_reference(
        &mut y_plane,
        &mut uv_plane,
        w,
        0,
        canvas_h,
        canvas,
        background,
        tiles,
        luts.as_ref(),
    )?;
    Nv12Image::new(canvas_w, canvas_h, y_plane, uv_plane, canvas.output_tag())
}

/// Fan the composite across `workers` scoped threads over even-row-aligned
/// bands. `total_pairs` is `canvas_h / 2`; each band owns `pairs_per_band`
/// chroma row-pairs (= `2 * pairs_per_band` luma rows) of a **disjoint** `&mut`
/// slice of the Y and UV planes, so there is no shared mutable state and the
/// borrow checker proves the split race-free. Returns the first band error in
/// row order (deterministic).
#[allow(clippy::too_many_arguments)]
// reason: the band slices plus the shared composite parameters; see
// `composite_band`. A struct would obscure the disjoint-`&mut` band ownership.
fn composite_parallel(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    w: usize,
    total_pairs: usize,
    workers: usize,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    luts: Option<&LutSet>,
) -> Result<()> {
    // Even split of chroma row-pairs across workers (ceil), so every band is the
    // same size except possibly the last — which is exactly what `chunks_mut`
    // produces. The number of resulting bands may be < `workers`.
    let pairs_per_band = total_pairs.div_ceil(workers.max(1)).max(1);
    let y_band_len = pairs_per_band * 2 * w; // 2 luma rows per chroma pair
    let uv_band_len = pairs_per_band * w; // 1 interleaved UV row per chroma pair

    let y_bands = y_plane.chunks_mut(y_band_len);
    let uv_bands = uv_plane.chunks_mut(uv_band_len);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for (band_index, (band_y, band_uv)) in y_bands.zip(uv_bands).enumerate() {
            // Global top row of this band (luma rows): band_index * band height.
            // `pairs_per_band * 2` luma rows per band; the band's own height is
            // derived from its (possibly shorter, final) slice length.
            let py_start = u32::try_from(band_index * pairs_per_band * 2).unwrap_or(u32::MAX);
            let band_h = u32::try_from(band_y.len() / w.max(1)).unwrap_or(0);
            let handle = scope.spawn(move || {
                composite_band(
                    band_y, band_uv, w, py_start, band_h, canvas, background, tiles, luts,
                )
            });
            handles.push(handle);
        }
        // Join in spawn (row) order and return the first error encountered.
        let mut first_err: Option<Error> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(Error::Geometry(
                            "composite worker thread panicked".to_owned(),
                        ));
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })
}

/// Render the canvas rows `[py_start, py_start + band_h)` into band-local Y/UV
/// planes (`band_y` is `band_h` rows of `w` bytes; `band_uv` is `band_h/2` rows
/// of `w` bytes). The tile/canvas coordinate space is the **global** canvas;
/// `py_start` rebases the band-local row index to the global row used for tile
/// addressing.
///
/// This is the byte-exact **oracle** kernel: pixel-driven, testing every tile at
/// every band pixel. It is O(pixels × all-tiles) and is kept only as the
/// reference the tile-driven production [`composite_band`] is pinned against
/// (see [`composite_reference`]); the production path no longer uses it.
///
/// It runs the front half (`tile_yuv_to_canvas_linear`), the premultiplied
/// linear `over`, and the back half (`canvas_linear_to_output_yuv`) — via the
/// LUT when `luts` is `Some`, else the oracle.
#[allow(clippy::too_many_arguments)]
// reason: this is the internal band kernel; the arguments are the band slices
// plus the shared composite parameters. Grouping them into a struct would not
// reduce the surface and would obscure the disjoint-`&mut` band ownership that
// makes the parallel split race-free.
fn composite_band_reference(
    band_y: &mut [u8],
    band_uv: &mut [u8],
    w: usize,
    py_start: u32,
    band_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    luts: Option<&LutSet>,
) -> Result<()> {
    let bg_premul = background.premultiplied();
    for py_local in 0..band_h {
        let py = py_start.saturating_add(py_local);
        for px in 0..u32::try_from(w).unwrap_or(u32::MAX) {
            // Fold the covering tiles back-to-front in linear light.
            let mut acc = bg_premul;
            for tile in tiles {
                let Some((sx, sy)) = tile_local_coords(tile, px, py) else {
                    continue;
                };
                let Some((y8, cb8, cr8)) = tile.image.sample(sx, sy) else {
                    continue;
                };
                let lin = match luts {
                    Some(lut) => {
                        lut.tile_yuv_to_canvas_linear(y8, cb8, cr8, tile.image.color(), canvas)?
                    }
                    None => tile_yuv_to_canvas_linear(y8, cb8, cr8, tile.image.color(), canvas)?,
                };
                let src = LinearRgba {
                    r: lin[0],
                    g: lin[1],
                    b: lin[2],
                    a: tile.opacity.clamp(0.0, 1.0),
                }
                .premultiplied();
                acc = over(src, acc);
            }
            let straight = PremulRgba {
                r: acc.r,
                g: acc.g,
                b: acc.b,
                a: acc.a,
            }
            .unpremultiplied();
            let out = match luts {
                Some(lut) => {
                    lut.canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?
                }
                None => canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?,
            };
            write_pixel(band_y, band_uv, w, px, py_local, out);
        }
    }
    Ok(())
}

/// Render the canvas rows `[py_start, py_start + band_h)` into band-local Y/UV
/// planes — the **tile-driven** production kernel (the optimized replacement for
/// [`composite_band_reference`]). `band_y` is `band_h` rows of `w` bytes;
/// `band_uv` is `band_h/2` rows of `w` bytes. The tile/canvas coordinate space
/// is the **global** canvas; `py_start` rebases the band-local row index to the
/// global row used for tile addressing.
///
/// This is the single per-pixel pipeline used by both the serial path and the
/// parallel bands. It is **byte-identical** to [`composite_band_reference`] for
/// every tile stack (pinned by `tests/composite_tile_driven.rs`) but does
/// O(pixels + Σ tile-area-in-band) work instead of O(pixels × all-tiles):
///
/// 1. Encode the background straight color through the back half **once**
///    (killing the per-uncovered-pixel colour round-trip) and fill the entire
///    band with that constant.
/// 2. Maintain a band-sized accumulator of premultiplied linear `bg`, then for
///    each tile in slice order (back-to-front) fold `over` into **only** the
///    pixels in its `rect ∩ band` — so each covered pixel sees the identical
///    per-pixel fold sequence as the reference, in the identical order.
/// 3. Re-encode through the back half **only** the pixels at least one tile
///    touched, leaving the precomputed background constant for the rest.
///
/// Invariant #5 (NV12-throughout): the output stays NV12 and no per-*tile* RGBA
/// is materialised. The tile-driven fold does hold a per-pixel
/// premultiplied-linear accumulator, but it is sized to the band's **covered row
/// range** ([`covered_row_span`]) — the even-row-aligned union of the rows any
/// tile touches — not the full band. So even on the single-threaded path (one
/// full-canvas band) the scratch is `O(covered_rows × width)`: a canvas with a
/// few small tiles never allocates a full-frame buffer, and an all-background band
/// allocates none at all. Rows outside the span keep the precomputed background
/// constant the fill already wrote. The covered span is even-row-aligned so a
/// 2×2 chroma block never straddles its boundary (NV12 chroma is 2×2
/// subsampled). Invariant #8 (fixed colour order) is unchanged: the same
/// front-half/`over`/back-half order runs; only the *iteration* changed.
#[allow(clippy::too_many_arguments)]
// reason: this is the internal band kernel; the arguments are the band slices
// plus the shared composite parameters. Grouping them into a struct would not
// reduce the surface and would obscure the disjoint-`&mut` band ownership that
// makes the parallel split race-free.
fn composite_band(
    band_y: &mut [u8],
    band_uv: &mut [u8],
    w: usize,
    py_start: u32,
    band_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    luts: Option<&LutSet>,
) -> Result<()> {
    let band_rows = usize::try_from(band_h).unwrap_or(0);
    if band_rows == 0 || w == 0 {
        return Ok(());
    }

    // 1. Background-encoded constant, computed ONCE (critic finding #3).
    let bg_premul = background.premultiplied();
    let bg_straight = PremulRgba {
        r: bg_premul.r,
        g: bg_premul.g,
        b: bg_premul.b,
        a: bg_premul.a,
    }
    .unpremultiplied();
    let bg_yuv = match luts {
        Some(lut) => {
            lut.canvas_linear_to_output_yuv([bg_straight.r, bg_straight.g, bg_straight.b], canvas)?
        }
        None => canvas_linear_to_output_yuv([bg_straight.r, bg_straight.g, bg_straight.b], canvas)?,
    };
    fill_band_solid(band_y, band_uv, w, band_rows, bg_yuv);

    // 2. Size the premultiplied accumulator to the band's COVERED row range
    //    (even-row-aligned union of every tile's rows ∩ band), not the full
    //    band — inv #5: the scratch is O(covered_rows × width), never the whole
    //    frame. Rows outside the span keep the background constant the fill
    //    wrote, so an all-background band allocates no accumulator at all.
    let Some((span_start, span_end)) = covered_row_span(band_rows, py_start, tiles) else {
        return Ok(()); // no tile touches this band: it is all background
    };
    let span_rows = span_end.saturating_sub(span_start);
    let span_pixels = span_rows.saturating_mul(w);

    // 3. Per-pixel premultiplied accumulator over the covered span (all
    //    background), plus a coverage bitmap of which pixels at least one tile
    //    touched. Fold each tile's rect ∩ span in slice order (back-to-front);
    //    `span_start` rebases band-local rows into accumulator rows.
    let mut acc = vec![bg_premul; span_pixels];
    let mut covered = vec![false; span_pixels];
    for tile in tiles {
        fold_tile_into_band(
            &mut acc,
            &mut covered,
            w,
            span_start,
            span_rows,
            py_start,
            tile,
            canvas,
            luts,
        )?;
    }

    // 4. Re-encode through the back half ONLY the pixels a tile touched; the
    //    rest already hold the precomputed background constant.
    encode_covered_pixels(
        band_y, band_uv, w, span_start, span_rows, &acc, &covered, canvas, luts,
    )
}

/// The band-local **covered row range** `[start, end)` (half-open) of the rows
/// any tile in `tiles` touches within a band of `band_rows` rows whose global
/// top row is `py_start` — the extent the [`composite_band`] accumulator is
/// sized to. Returns [`None`] when no tile overlaps the band (the whole band is
/// background, so no accumulator is allocated).
///
/// The range is **even-row-aligned** (`start` floored to even, `end` ceiled to
/// even) and clamped to `[0, band_rows]`. Even alignment is load-bearing: NV12
/// chroma is 2×2 subsampled, so a 2×2 block must lie wholly inside or wholly
/// outside the accumulator (the back-half chroma write keys off the block's
/// bottom-right pixel — see [`encode_covered_pixels`]).
///
/// **Precondition:** `band_rows` is even. Every production caller honours this —
/// the band split keeps each band an even number of luma rows and the serial
/// path passes the validated-even `canvas_h` — so ceiling `end` to even never
/// truncates a touched row. (The `.min(band_rows)` clamp is defensive: for an
/// out-of-contract odd `band_rows` the returned `end` may be the odd
/// `band_rows`, which is the only safe in-bounds value.)
#[doc(hidden)]
#[must_use]
pub fn covered_row_span(
    band_rows: usize,
    py_start: u32,
    tiles: &[Tile<'_>],
) -> Option<(usize, usize)> {
    if band_rows == 0 {
        return None;
    }
    let band_top = i64::from(py_start);
    let band_bottom = band_top.saturating_add(i64::try_from(band_rows).unwrap_or(0)); // exclusive

    let mut min_local: Option<usize> = None;
    let mut max_local_excl: usize = 0;
    for tile in tiles {
        let ty0 = i64::from(tile.dst_y);
        // The tile occupies its DESTINATION rect (scale-at-composite), so the
        // covered rows span `dst_h`, not the source image height.
        let ty1 = ty0.saturating_add(i64::from(tile.eff_dst_h()));
        // The tile's global row range ∩ the band.
        let lo = ty0.max(band_top);
        let hi = ty1.min(band_bottom);
        if lo >= hi {
            continue; // disjoint from this band (or zero-height)
        }
        // Band-local, in-range by construction (both clamped to the band).
        let (Ok(local_lo), Ok(local_hi)) = (
            usize::try_from(lo - band_top),
            usize::try_from(hi - band_top),
        ) else {
            continue;
        };
        min_local = Some(min_local.map_or(local_lo, |m| m.min(local_lo)));
        max_local_excl = max_local_excl.max(local_hi);
    }

    let start_raw = min_local?;
    // Floor start to even, ceil end to even, clamp to the band.
    let start = start_raw & !1;
    let end = max_local_excl.saturating_add(1) & !1; // ceil to even
    let end = end.min(band_rows);
    if start >= end {
        return None;
    }
    Some((start, end))
}

/// Fold one tile's `rect ∩ accumulator-span` into the premultiplied accumulator
/// `acc` (back-to-front `over`), marking `covered` for each touched pixel. The
/// accumulator covers `span_rows` band-local rows starting at band-local row
/// `span_start`; `py_start` is the band's global top row. Only the pixels inside
/// the tile's clipped destination rect are visited — the tile-driven inner loop
/// (vs. the reference's per-pixel-all-tiles scan).
#[allow(clippy::too_many_arguments)]
// reason: the band accumulator + coverage, the band geometry, and the tile +
// shared composite parameters; a struct would not shrink the surface.
fn fold_tile_into_band(
    acc: &mut [PremulRgba],
    covered: &mut [bool],
    w: usize,
    span_start: usize,
    span_rows: usize,
    py_start: u32,
    tile: &Tile<'_>,
    canvas: CanvasColor,
    luts: Option<&LutSet>,
) -> Result<()> {
    // Global top/bottom (exclusive) of the accumulator's covered span.
    let band_top = i64::from(py_start);
    let span_top = band_top.saturating_add(i64::try_from(span_start).unwrap_or(0));
    let span_bottom = span_top.saturating_add(i64::try_from(span_rows).unwrap_or(0)); // exclusive
    let w_i64 = i64::try_from(w).unwrap_or(i64::MAX);

    let dst_x = i64::from(tile.dst_x);
    let dst_y = i64::from(tile.dst_y);
    // The tile occupies its DESTINATION rect; the source is scaled into it
    // (scale-at-composite). The rect extent is `dst_w`/`dst_h`, not the source
    // image size — they coincide for a 1:1 tile.
    let tile_w = i64::from(tile.eff_dst_w());
    let tile_h = i64::from(tile.eff_dst_h());

    // Global x range covered: [dst_x, dst_x + tile_w) ∩ [0, w).
    let gx0 = dst_x.max(0);
    let gx1 = dst_x.saturating_add(tile_w).min(w_i64);
    // Global y range covered: [dst_y, dst_y + tile_h) ∩ [span_top, span_bottom).
    let gy0 = dst_y.max(span_top);
    let gy1 = dst_y.saturating_add(tile_h).min(span_bottom);
    if gx0 >= gx1 || gy0 >= gy1 {
        return Ok(()); // disjoint from the accumulator span (or fully off-canvas)
    }
    let opacity = tile.opacity.clamp(0.0, 1.0);

    let mut gy = gy0;
    while gy < gy1 {
        // Accumulator-local row + the destination-rect-local row offset `ddy`.
        // Both ranges are derived from the same clip, so the conversions below
        // are in-range by construction.
        let (Ok(acc_row), Ok(ddy)) = (usize::try_from(gy - span_top), u32::try_from(gy - dst_y))
        else {
            break;
        };
        let row_base = acc_row.saturating_mul(w);
        let mut gx = gx0;
        while gx < gx1 {
            // Bands split only rows, so the band-local column == global x; `ddx`
            // is the destination-rect-local column offset.
            let (Ok(col), Ok(ddx)) = (usize::try_from(gx), u32::try_from(gx - dst_x)) else {
                gx += 1;
                continue;
            };
            // Scale-at-composite: map the destination-rect offset to the source
            // pixel (nearest-neighbour; identity when dst size == src size).
            let (src_x, src_y) = tile.src_pixel_for(ddx, ddy);
            if let Some((y8, cb8, cr8)) = tile.image.sample(src_x, src_y) {
                let lin = match luts {
                    Some(lut) => {
                        lut.tile_yuv_to_canvas_linear(y8, cb8, cr8, tile.image.color(), canvas)?
                    }
                    None => tile_yuv_to_canvas_linear(y8, cb8, cr8, tile.image.color(), canvas)?,
                };
                let src = LinearRgba {
                    r: lin[0],
                    g: lin[1],
                    b: lin[2],
                    a: opacity,
                }
                .premultiplied();
                if let Some(slot) = acc.get_mut(row_base + col) {
                    *slot = over(src, *slot);
                }
                if let Some(flag) = covered.get_mut(row_base + col) {
                    *flag = true;
                }
            }
            gx += 1;
        }
        gy += 1;
    }
    Ok(())
}

/// Encode the back half (`canvas_linear_to_output_yuv`) for ONLY the band pixels
/// a tile touched, writing into the band Y/UV planes (uncovered pixels keep the
/// precomputed background constant the fill wrote). The accumulator covers
/// `span_rows` band-local rows starting at band-local row `span_start`; each
/// accumulator row `r` maps to band-local row `span_start + r`.
///
/// Y is per-pixel independent: write the encoded luma of every covered pixel.
/// Chroma is 4:2:0, and the reference's `write_pixel` writes the same UV slot
/// for all 4 pixels of a 2×2 block in raster order — so a block's final chroma
/// is whatever its **bottom-right** pixel (band-local row odd, col odd, the last
/// written) produced. We reproduce that exactly: write a block's chroma only
/// when its bottom-right pixel is covered (an uncovered bottom-right pixel would
/// re-emit the background chroma the fill already wrote — identical). This keeps
/// chroma byte-identical to the reference's last-writer-wins without re-encoding
/// uncovered pixels. `span_start` is even, so a band-local row's block parity
/// equals its accumulator row's parity and no 2×2 block straddles the span.
#[allow(clippy::too_many_arguments)]
// reason: the band slices + geometry + accumulator/coverage + shared composite
// parameters; a struct would not shrink the surface.
fn encode_covered_pixels(
    band_y: &mut [u8],
    band_uv: &mut [u8],
    w: usize,
    span_start: usize,
    span_rows: usize,
    acc: &[PremulRgba],
    covered: &[bool],
    canvas: CanvasColor,
    luts: Option<&LutSet>,
) -> Result<()> {
    for acc_row in 0..span_rows {
        let row_base = acc_row.saturating_mul(w);
        // Band-local row this accumulator row writes to.
        let Ok(row_u32) = u32::try_from(span_start.saturating_add(acc_row)) else {
            break;
        };
        let row_is_block_bottom = (span_start.saturating_add(acc_row)) % 2 == 1;
        for col in 0..w {
            let idx = row_base + col;
            let Some(true) = covered.get(idx).copied() else {
                continue;
            };
            let Some(&p) = acc.get(idx) else {
                continue;
            };
            let straight = PremulRgba {
                r: p.r,
                g: p.g,
                b: p.b,
                a: p.a,
            }
            .unpremultiplied();
            let out = match luts {
                Some(lut) => {
                    lut.canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?
                }
                None => canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?,
            };
            let Ok(col_u32) = u32::try_from(col) else {
                continue;
            };
            // Always write this pixel's luma.
            write_luma(band_y, w, col_u32, row_u32, out[0]);
            // Write chroma only for the bottom-right pixel of a 2×2 block — the
            // last writer in the reference's raster order.
            if row_is_block_bottom && col % 2 == 1 {
                write_chroma(band_uv, w, col_u32, row_u32, out[1], out[2]);
            }
        }
    }
    Ok(())
}

/// Fill an entire band's Y and interleaved UV planes with one solid output
/// `(y, cb, cr)` — the precomputed background constant (`band_rows` luma rows of
/// `w` bytes; `band_rows/2` interleaved UV rows of `w` bytes).
fn fill_band_solid(
    band_y: &mut [u8],
    band_uv: &mut [u8],
    w: usize,
    band_rows: usize,
    yuv: [u8; 3],
) {
    let y_len = band_rows.saturating_mul(w);
    if let Some(slice) = band_y.get_mut(..y_len.min(band_y.len())) {
        slice.fill(yuv[0]);
    }
    let uv_rows = band_rows / 2;
    let uv_len = uv_rows.saturating_mul(w);
    if let Some(slice) = band_uv.get_mut(..uv_len.min(band_uv.len())) {
        for pair in slice.chunks_exact_mut(2) {
            if let [cb, cr] = pair {
                *cb = yuv[1];
                *cr = yuv[2];
            }
        }
    }
}

/// Map a canvas pixel to the **source** coordinate the tile samples there, or
/// [`None`] if the pixel is outside the tile's destination rectangle.
///
/// The destination rect is `[dst_x, dst_x + dst_w) × [dst_y, dst_y + dst_h)`; a
/// pixel inside it maps to a source pixel via nearest-neighbour scaling
/// ([`Tile::src_pixel_for`]). When `dst_w`/`dst_h` equal the source size this is
/// the identity, so a 1:1 tile is byte-for-byte the prior behaviour.
fn tile_local_coords(tile: &Tile<'_>, px: u32, py: u32) -> Option<(u32, u32)> {
    let ddx = px.checked_sub(tile.dst_x)?;
    let ddy = py.checked_sub(tile.dst_y)?;
    if ddx >= tile.eff_dst_w() || ddy >= tile.eff_dst_h() {
        return None;
    }
    Some(tile.src_pixel_for(ddx, ddy))
}

/// Write one output pixel's `(y, cb, cr)` into the NV12 planes. Chroma is
/// written for every pixel (last-writer-wins within a 2x2 block, matching the
/// reference's nearest-neighbour model). Used by the reference (oracle) kernel;
/// the tile-driven kernel splits this into [`write_luma`] + [`write_chroma`].
fn write_pixel(y_plane: &mut [u8], uv_plane: &mut [u8], w: usize, px: u32, py: u32, yuv: [u8; 3]) {
    write_luma(y_plane, w, px, py, yuv[0]);
    write_chroma(uv_plane, w, px, py, yuv[1], yuv[2]);
}

/// Write one output pixel's luma into the Y plane (a no-op if out of bounds).
fn write_luma(y_plane: &mut [u8], w: usize, px: u32, py: u32, y: u8) {
    let (Ok(xi), Ok(yi)) = (usize::try_from(px), usize::try_from(py)) else {
        return;
    };
    if let Some(slot) = y_plane.get_mut(yi * w + xi) {
        *slot = y;
    }
}

/// Write one output pixel's chroma into the interleaved Cb/Cr plane at the 2×2
/// block containing `(px, py)` (a no-op if out of bounds). The reference's
/// nearest-neighbour model writes the same UV slot for every pixel of the block;
/// the tile-driven kernel calls this only for the block's bottom-right pixel so
/// the result matches the reference's last-writer-wins exactly.
fn write_chroma(uv_plane: &mut [u8], w: usize, px: u32, py: u32, cb: u8, cr: u8) {
    let (Ok(xi), Ok(yi)) = (usize::try_from(px), usize::try_from(py)) else {
        return;
    };
    let cx = xi / 2;
    let cy = yi / 2;
    let uv_index = (cy * w) + (cx * 2);
    if let Some(slot) = uv_plane.get_mut(uv_index) {
        *slot = cb;
    }
    if let Some(slot) = uv_plane.get_mut(uv_index + 1) {
        *slot = cr;
    }
}
