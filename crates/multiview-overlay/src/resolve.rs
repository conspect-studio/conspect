//! Resolving an [`OverlayStack`] into a
//! backend-agnostic draw list.
//!
//! Per ADR-R008 the overlay subsystem emits a *portable premultiplied-RGBA
//! atlas plus quad list* that any compositor backend (wgpu / Metal / CUDA-NPP)
//! can consume — there is no stable cross-vendor external-texture import, so
//! the contract is geometry + identity, not GPU handles. This module produces
//! the quad list: for each visible layer, the pixel-space destination
//! rectangle, the draw order, and the compositing parameters. The atlas/texture
//! for each quad is owned by the renderer (feature-gated crates); this is the
//! pure description of *what to draw and where*.

use crate::error::{Error, Result};
use crate::geometry::{f32_from_u32, NormRect, PixelRect, Region};
use crate::layer::{BlendMode, OverlayStack, Target};

/// The output canvas size in pixels. Both dimensions must be non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasSize {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
}

impl CanvasSize {
    /// Construct a canvas size.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Validate that both dimensions are non-zero.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCanvas`] if either dimension is zero.
    pub fn validate(self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidCanvas(format!(
                "dimensions must be > 0 (got {}x{})",
                self.width, self.height
            )));
        }
        Ok(())
    }
}

/// One resolved quad to composite: the layer it came from, its pixel-space
/// destination, and the compositing parameters. Emitted in back-to-front order.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawQuad {
    /// The source layer's id (so the renderer can look up its atlas entry).
    pub layer_id: String,
    /// Destination rectangle on the canvas, in pixels.
    pub dest: PixelRect,
    /// Effective opacity multiplier, already clamped into `0.0..=1.0`.
    pub opacity: f32,
    /// How to blend onto the canvas.
    pub blend: BlendMode,
}

/// The full, ordered set of quads to draw for one frame. Back-to-front: index
/// `0` is drawn first (furthest back), the last is drawn on top.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrawList {
    /// The quads, in draw (back-to-front) order.
    pub quads: Vec<DrawQuad>,
}

impl OverlayStack {
    /// Resolve every visible layer against `canvas` into a back-to-front
    /// [`DrawList`].
    ///
    /// Validates the canvas, rejects duplicate layer ids, and for each visible
    /// layer validates its target/region rectangles, maps them to pixels, and
    /// anchors the layer box inside its placement region. Invisible layers are
    /// skipped (but still checked for id uniqueness).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidCanvas`] if a canvas dimension is zero.
    /// - [`Error::DuplicateLayerId`] if two layers share an `id`.
    /// - [`Error::InvalidRect`] if a layer's tile target or placement region is
    ///   out of range.
    pub fn resolve(&self, canvas: CanvasSize) -> Result<DrawList> {
        canvas.validate()?;
        self.check_unique_ids()?;

        let mut quads = Vec::new();
        for layer in self.draw_order() {
            if !layer.visible {
                continue;
            }

            // The surface region (full canvas or the bound tile) in pixels.
            let surface = match &layer.target {
                Target::FullCanvas => Region::from_pixels(
                    0.0,
                    0.0,
                    f32_from_u32(canvas.width),
                    f32_from_u32(canvas.height),
                ),
                Target::Tile { rect } => {
                    rect.validate()?;
                    rect.to_region(canvas.width, canvas.height)
                }
            };

            // The placement region is a normalized sub-region of the surface.
            layer.placement.region.validate()?;
            let region = sub_region(surface, layer.placement.region);

            let dest =
                layer
                    .placement
                    .anchor
                    .place(region, layer.placement.size, layer.placement.padding);

            quads.push(DrawQuad {
                layer_id: layer.id.clone(),
                dest,
                opacity: layer.opacity.clamp(0.0, 1.0),
                blend: layer.blend,
            });
        }
        Ok(DrawList { quads })
    }

    /// Reject any two layers sharing an `id`.
    fn check_unique_ids(&self) -> Result<()> {
        let layers = self.layers();
        for (i, layer) in layers.iter().enumerate() {
            if layers.iter().take(i).any(|earlier| earlier.id == layer.id) {
                return Err(Error::DuplicateLayerId(layer.id.clone()));
            }
        }
        Ok(())
    }
}

/// Map a validated [`NormRect`] onto a parent [`Region`] (the surface), so a
/// placement region nests inside its target.
fn sub_region(parent: Region, norm: NormRect) -> Region {
    Region::from_pixels(
        parent.x + norm.x * parent.width,
        parent.y + norm.y * parent.height,
        norm.w * parent.width,
        norm.h * parent.height,
    )
}
