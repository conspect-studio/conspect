//! The overlay text engine: string + font + px size + color → shaped, cached,
//! premultiplied-RGBA glyph coverage (ADR-0016 §3.2, overlay-rendering.md §3.2).
//!
//! [`TextEngine`] owns the deterministic bundled-font [`cosmic_text::FontSystem`],
//! a [`cosmic_text::SwashCache`] (per-glyph rasterization cache), and the bounded
//! [`GlyphAtlas`]. It exposes the two consumers that share **one** rasterizer so
//! the GPU fast path and the CPU reference never diverge on metrics
//! (overlay-rendering.md §4.4):
//!
//! - [`TextEngine::prepare_run`] — shape a run, ensure every glyph is resident in
//!   the atlas (inserting only genuinely new glyphs, T2), and return atlas-relative
//!   [`PreparedGlyph`] quads for the GPU sub-pass.
//! - [`TextEngine::rasterize_run`] — return each glyph's **premultiplied RGBA**
//!   coverage bitmap + destination rect ([`RasterizedGlyph`]) for the CPU
//!   reference blit into the linear canvas.
//!
//! swash emits **straight 8-bit coverage** (`SwashContent::Mask`); we
//! premultiply (`rgb * coverage, coverage`) here so the bytes are ready for the
//! one premultiplied source-over blend (invariant #8 / ADR-R008). Color emoji
//! (`SwashContent::Color`/`SubpixelMask`) are out of scope for Stage 1 and
//! skipped (a label glyph that resolves to a color bitmap simply does not ink).

use cosmic_text::{Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache};
use cosmic_text::{SwashContent, SwashImage};

use crate::error::Result;
use crate::overlay::atlas::{AtlasSlot, GlyphAtlas};
use crate::overlay::fonts::{self, MONO_FAMILY, SANS_FAMILY};

/// Which bundled OFL face to shape with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FontFamily {
    /// `JetBrains` Mono — monospaced, for digits/timecode (column alignment).
    Mono,
    /// Noto Sans — proportional, broad-Latin, for labels.
    Sans,
}

impl FontFamily {
    /// The `cosmic-text` family name this maps to (one of the bundled faces).
    const fn family_name(self) -> &'static str {
        match self {
            FontFamily::Mono => MONO_FAMILY,
            FontFamily::Sans => SANS_FAMILY,
        }
    }
}

/// One shaped, atlas-resident glyph: where to draw it on the canvas (`dest_*`,
/// pixels, top-left origin) and where its premultiplied coverage lives in the
/// atlas ([`AtlasSlot`]). The GPU sub-pass turns each of these into a textured
/// quad that samples the atlas and blends `over` the linear canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedGlyph {
    /// Atlas-relative location of this glyph's premultiplied coverage.
    pub slot: AtlasSlot,
    /// Destination X of the glyph box top-left, in canvas pixels (run origin
    /// `(0, 0)` at the pen start / baseline left).
    pub dest_x: i32,
    /// Destination Y of the glyph box top-left, in canvas pixels (relative to the
    /// run baseline; positive is downward).
    pub dest_y: i32,
    /// Glyph coverage box width, in pixels.
    pub width: u32,
    /// Glyph coverage box height, in pixels.
    pub height: u32,
}

/// The result of [`TextEngine::prepare_run`]: the run's atlas-resident glyph
/// quads plus the straight (non-premultiplied) fill color, ready for the GPU
/// overlay sub-pass (which samples each glyph's coverage from the atlas and
/// premultiplies `color * coverage` at draw time — ADR-R008).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreparedRun {
    glyphs: Vec<PreparedGlyph>,
    color: [f32; 4],
}

impl PreparedRun {
    /// The placed glyph quads (skipping whitespace / zero-coverage glyphs).
    #[must_use]
    pub fn glyphs(&self) -> &[PreparedGlyph] {
        &self.glyphs
    }

    /// The run's straight (non-premultiplied) RGBA fill color, `0.0..=1.0`. The
    /// GPU sub-pass multiplies it by each sampled coverage and premultiplies.
    #[must_use]
    pub const fn color(&self) -> [f32; 4] {
        self.color
    }
}

/// One rasterized glyph for the CPU reference path: a premultiplied-RGBA coverage
/// bitmap plus its destination rect. The CPU compositor blits `premultiplied_rgba`
/// at `(dest_x, dest_y)` into the linear canvas with [`crate::blend::over`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterizedGlyph {
    /// Destination X of the glyph box top-left, in canvas pixels.
    pub dest_x: i32,
    /// Destination Y of the glyph box top-left, in canvas pixels.
    pub dest_y: i32,
    /// Glyph coverage box width, in pixels.
    pub width: u32,
    /// Glyph coverage box height, in pixels.
    pub height: u32,
    /// Premultiplied RGBA8, row-major, `width * height * 4` bytes
    /// (`rgb * coverage, coverage`).
    pub premultiplied_rgba: Vec<u8>,
}

/// The result of [`TextEngine::rasterize_run`]: the run's premultiplied glyph
/// bitmaps for the CPU reference blit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RasterizedRun {
    glyphs: Vec<RasterizedGlyph>,
}

impl RasterizedRun {
    /// The rasterized glyphs (skipping whitespace / zero-coverage glyphs).
    #[must_use]
    pub fn glyphs(&self) -> &[RasterizedGlyph] {
        &self.glyphs
    }
}

/// The overlay text engine: shapes runs with the bundled fonts and manages the
/// bounded glyph atlas (ADR-0016 §3.2).
pub struct TextEngine {
    font_system: FontSystem,
    swash: SwashCache,
    atlas: GlyphAtlas,
}

impl TextEngine {
    /// Default atlas byte cap: a 1024×1024 premultiplied-RGBA atlas
    /// (`1024 * 1024 * 4` = 4 MiB). We cap resident glyph bytes at that so the
    /// engine is bounded out of the box (T4). Callers with tighter budgets use
    /// [`TextEngine::with_atlas_byte_cap`].
    pub const DEFAULT_ATLAS_BYTE_CAP: usize = 4 * 1024 * 1024;

    /// Build the engine with the default atlas byte cap.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::FontLoad`] if the bundled OFL fonts cannot be loaded.
    pub fn new() -> Result<Self> {
        Self::with_atlas_byte_cap(Self::DEFAULT_ATLAS_BYTE_CAP)
    }

    /// Build the engine with an explicit atlas byte cap (T4 bounded memory).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::FontLoad`] if the bundled OFL fonts cannot be loaded.
    pub fn with_atlas_byte_cap(byte_cap: usize) -> Result<Self> {
        let font_system = fonts::build_font_system()?;
        Ok(Self {
            font_system,
            swash: SwashCache::new(),
            atlas: GlyphAtlas::new(GlyphAtlas::DEFAULT_SIDE, byte_cap),
        })
    }

    /// Number of glyphs currently resident in the atlas (T2/T4 assertions).
    #[must_use]
    pub fn atlas_entry_count(&self) -> usize {
        self.atlas.entry_count()
    }

    /// Total premultiplied-RGBA bytes the atlas currently holds (T4 cap).
    #[must_use]
    pub fn atlas_used_bytes(&self) -> usize {
        self.atlas.used_bytes()
    }

    /// The atlas's hard byte cap.
    #[must_use]
    pub fn atlas_byte_cap(&self) -> usize {
        self.atlas.byte_cap()
    }

    /// Shape `text` in `family` at `size_px`, ensure every inked glyph is resident
    /// in the atlas, and return the run's atlas-relative glyph quads.
    ///
    /// Per-glyph caching means an unchanged or glyph-overlapping re-render inserts
    /// **only** genuinely new glyphs (T2). `color` is straight (non-premultiplied)
    /// RGBA in `0.0..=1.0`; it is recorded on the prepared quads for the GPU
    /// sub-pass to premultiply at sample time. Whitespace / zero-coverage glyphs
    /// are skipped (they emit no quad).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::AtlasGlyphTooLarge`] if a single glyph's coverage cannot
    /// fit the atlas even when empty.
    pub fn prepare_run(
        &mut self,
        text: &str,
        family: FontFamily,
        size_px: f32,
        color: [f32; 4],
    ) -> Result<PreparedRun> {
        let shaped = self.shape(text, family, size_px);
        let mut glyphs = Vec::with_capacity(shaped.len());
        for placed in shaped {
            let Some(resident) = self.ensure_resident(&placed)? else {
                continue;
            };
            glyphs.push(PreparedGlyph {
                slot: resident.slot,
                dest_x: resident.dest_x,
                dest_y: resident.dest_y,
                width: resident.slot.width,
                height: resident.slot.height,
            });
        }
        Ok(PreparedRun { glyphs, color })
    }

    /// Shape `text` in `family` at `size_px` and return each inked glyph as a
    /// **premultiplied-RGBA** coverage bitmap + destination rect for the CPU
    /// reference blit.
    ///
    /// This shares the **same** shaping + swash rasterization as
    /// [`Self::prepare_run`] (one rasterizer, two consumers — no metric
    /// divergence). `color` is straight RGBA in `0.0..=1.0`; the returned bytes
    /// are premultiplied (`round(c * 255) * coverage / 255`, `coverage`).
    /// Whitespace / zero-coverage glyphs are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::AtlasGlyphTooLarge`] if a single glyph's coverage cannot
    /// fit the atlas even when empty (the same residency path as `prepare_run`).
    pub fn rasterize_run(
        &mut self,
        text: &str,
        family: FontFamily,
        size_px: f32,
        color: [f32; 4],
    ) -> Result<RasterizedRun> {
        let rgb8 = straight_rgb8(color);
        let shaped = self.shape(text, family, size_px);
        let mut glyphs = Vec::with_capacity(shaped.len());
        for placed in shaped {
            let Some(resident) = self.ensure_resident(&placed)? else {
                continue;
            };
            let premultiplied_rgba = premultiply_mask(&resident.image, rgb8);
            glyphs.push(RasterizedGlyph {
                dest_x: resident.dest_x,
                dest_y: resident.dest_y,
                width: resident.slot.width,
                height: resident.slot.height,
                premultiplied_rgba,
            });
        }
        Ok(RasterizedRun { glyphs })
    }

    /// Shape one run into placed glyphs (cache key + integer pen position).
    fn shape(&mut self, text: &str, family: FontFamily, size_px: f32) -> Vec<PlacedGlyph> {
        // Line height is irrelevant for single-run placement; use the size.
        let metrics = Metrics::new(size_px, size_px);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let attrs = Attrs::new().family(Family::Name(family.family_name()));
        buffer.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);

        let mut placed = Vec::new();
        for run in buffer.layout_runs() {
            let baseline = run.line_y;
            for glyph in run.glyphs {
                // Physical (pixel-snapped) placement at the run baseline; scale 1
                // because size_px is already device pixels (invariant #6).
                let physical = glyph.physical((0.0, baseline), 1.0);
                placed.push(PlacedGlyph {
                    key: physical.cache_key,
                    pen_x: physical.x,
                    pen_y: physical.y,
                });
            }
        }
        placed
    }

    /// Ensure the shaped glyph is rasterized and resident in the atlas, and
    /// compute its canvas destination rect from the swash placement offset.
    ///
    /// Returns `None` for a whitespace / zero-area / non-mask (color emoji)
    /// glyph, which inks nothing and is skipped by both consumers.
    fn ensure_resident(&mut self, placed: &PlacedGlyph) -> Result<Option<ResidentGlyph>> {
        let Some(image) = self
            .swash
            .get_image(&mut self.font_system, placed.key)
            .clone()
        else {
            return Ok(None);
        };
        // Stage 1 inks only straight-coverage masks; color/subpixel bitmaps are a
        // later stage and are treated as non-inking here.
        if image.content != SwashContent::Mask {
            return Ok(None);
        }
        // swash placement: width/height are u32 texels; left/top are i32 offsets
        // from the pen (top is upward-positive).
        let width = image.placement.width;
        let height = image.placement.height;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let slot = self.atlas.insert(placed.key, width, height)?;
        let dest_x = placed.pen_x.saturating_add(image.placement.left);
        let dest_y = placed.pen_y.saturating_sub(image.placement.top);
        Ok(Some(ResidentGlyph {
            slot,
            dest_x,
            dest_y,
            image,
        }))
    }
}

/// A shaped glyph placed at an integer pen position, ready for residency.
struct PlacedGlyph {
    key: CacheKey,
    pen_x: i32,
    pen_y: i32,
}

/// A glyph that is resident in the atlas, with its canvas destination computed.
struct ResidentGlyph {
    slot: AtlasSlot,
    dest_x: i32,
    dest_y: i32,
    image: SwashImage,
}

/// Quantize a straight `0.0..=1.0` RGBA color's RGB to 8-bit (alpha comes from
/// per-pixel coverage). Out-of-range channels are clamped.
fn straight_rgb8(color: [f32; 4]) -> [u8; 3] {
    [
        quantize_unit(color[0]),
        quantize_unit(color[1]),
        quantize_unit(color[2]),
    ]
}

/// Clamp a unit float to `[0, 1]` and quantize to `0..=255` (round-to-nearest),
/// without lossy `as` casts.
///
/// The result is found by binary-searching the 256 `u8` values for the one whose
/// `f32` image is nearest the scaled input — a closed, total mapping that needs
/// no `f32 -> integer` cast (banned by the workspace lints). It runs at most 8
/// comparisons and only on a text/color change, never on the hot path.
fn quantize_unit(v: f32) -> u8 {
    let scaled = (v.clamp(0.0, 1.0) * 255.0).round();
    let (mut lo, mut hi): (u8, u8) = (0, u8::MAX);
    while lo < hi {
        // Midpoint without overflow; `lo <= mid < hi`.
        let mid = lo + (hi - lo) / 2;
        if f32::from(mid) < scaled {
            lo = mid.saturating_add(1);
        } else {
            hi = mid;
        }
    }
    lo
}

/// Premultiply a straight-coverage swash mask by the run's RGB into a
/// row-major premultiplied-RGBA8 buffer (`rgb * coverage / 255, coverage`).
///
/// swash mask data is one coverage byte per pixel, row-major over the placement
/// box; the output is `width * height * 4` bytes.
fn premultiply_mask(image: &SwashImage, rgb8: [u8; 3]) -> Vec<u8> {
    let w = usize::try_from(image.placement.width).unwrap_or(0);
    let h = usize::try_from(image.placement.height).unwrap_or(0);
    let [red, green, blue] = rgb8;
    let mut out = vec![0_u8; w.saturating_mul(h).saturating_mul(4)];
    for (dst, &cov) in out.chunks_exact_mut(4).zip(image.data.iter()) {
        // premultiplied channel = round(c * cov / 255), with c, cov in 0..=255.
        if let [out_r, out_g, out_b, out_a] = dst {
            *out_r = mul_u8(red, cov);
            *out_g = mul_u8(green, cov);
            *out_b = mul_u8(blue, cov);
            *out_a = cov;
        }
    }
    out
}

/// Multiply two 8-bit values as if in `[0, 1]`, rounded: `round(a * b / 255)`.
fn mul_u8(a: u8, b: u8) -> u8 {
    let product = u32::from(a) * u32::from(b) + 127;
    // product/255 is in 0..=255 for a,b in 0..=255; division stays in range.
    let scaled = product / 255;
    u8::try_from(scaled).unwrap_or(u8::MAX)
}
