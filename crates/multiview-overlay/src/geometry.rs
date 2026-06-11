//! Pure box/anchor layout math.
//!
//! An overlay is positioned in two stages, both pure and exactly testable:
//!
//! 1. A [`NormRect`] (normalized `0.0..=1.0`, exactly like a layout
//!    [`multiview_core::layout::Cell`]) is resolved against a canvas size into a
//!    pixel-space [`Region`].
//! 2. A fixed-size box ([`BoxSize`]) is placed inside that [`Region`] by an
//!    [`Anchor`] (9-point) with edge [`Padding`], yielding a [`PixelRect`] the
//!    compositor draws into.
//!
//! No GPU, no rasterizer, no floating-point fps — just deterministic geometry.
//! Sub-pixel (`f32`) results are intentional: the premultiplied-alpha
//! compositor (ADR-R008) positions quads with sub-pixel precision.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A normalized rectangle on a target surface, with each field in `0.0..=1.0`.
///
/// `x`/`y` are the top-left origin as a fraction of the surface; `w`/`h` are the
/// extent as a fraction. Mirrors [`multiview_core::layout::Cell`]'s coordinate
/// convention so overlay regions and tiles compose cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormRect {
    /// Left edge (fraction of surface width).
    pub x: f32,
    /// Top edge (fraction of surface height).
    pub y: f32,
    /// Width (fraction of surface width).
    pub w: f32,
    /// Height (fraction of surface height).
    pub h: f32,
}

impl NormRect {
    /// The whole surface: `(0, 0, 1, 1)`.
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    /// Construct a normalized rectangle. Validate with [`NormRect::validate`]
    /// before resolving to pixels.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Check the structural invariants: all fields finite, origin within
    /// `0.0..=1.0`, positive extent, and the far edges (`x + w`, `y + h`) not
    /// exceeding `1.0`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRect`] describing the first violated invariant.
    pub fn validate(self) -> Result<()> {
        for (name, value) in [("x", self.x), ("y", self.y), ("w", self.w), ("h", self.h)] {
            if !value.is_finite() {
                return Err(Error::InvalidRect(format!("{name} must be finite")));
            }
        }
        if self.w <= 0.0 || self.h <= 0.0 {
            return Err(Error::InvalidRect(format!(
                "extent must be > 0 (w={}, h={})",
                self.w, self.h
            )));
        }
        if self.x < 0.0 || self.y < 0.0 {
            return Err(Error::InvalidRect(format!(
                "origin ({}, {}) must be within 0.0..=1.0",
                self.x, self.y
            )));
        }
        if self.x + self.w > 1.0 {
            return Err(Error::InvalidRect(format!(
                "x + w = {} exceeds 1.0",
                self.x + self.w
            )));
        }
        if self.y + self.h > 1.0 {
            return Err(Error::InvalidRect(format!(
                "y + h = {} exceeds 1.0",
                self.y + self.h
            )));
        }
        Ok(())
    }

    /// Resolve this normalized rectangle against a canvas size into a pixel
    /// [`Region`]. Does **not** validate; call [`NormRect::validate`] first when
    /// the value is untrusted.
    #[must_use]
    pub fn to_region(self, canvas_width: u32, canvas_height: u32) -> Region {
        let cw = f32_from_u32(canvas_width);
        let ch = f32_from_u32(canvas_height);
        Region {
            x: self.x * cw,
            y: self.y * ch,
            width: self.w * cw,
            height: self.h * ch,
        }
    }
}

/// Lossless-enough `u32 -> f32` for canvas/surface dimensions.
///
/// Canvas sizes are small (`<= 8192` in practice, always `< 2^24`), so the
/// conversion is exact. The intermediate `u16` split keeps every step inside the
/// lossless `u16 -> f32` domain via [`f32::from`], avoiding an `as` cast (banned
/// by the workspace lint policy) without precision loss.
pub(crate) fn f32_from_u32(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// A pixel-space rectangle (top-left origin, width/height), in `f32` so the
/// compositor keeps sub-pixel precision for premultiplied-alpha placement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PixelRect {
    /// Left edge in pixels.
    pub x: f32,
    /// Top edge in pixels.
    pub y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
}

impl PixelRect {
    /// The right edge (`x + width`).
    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.width
    }

    /// The bottom edge (`y + height`).
    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.height
    }
}

/// A resolved pixel-space region (a [`NormRect`] mapped onto a canvas, or a tile
/// rectangle). Anchored placement happens inside one of these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    /// Left edge in pixels.
    pub x: f32,
    /// Top edge in pixels.
    pub y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
}

impl Region {
    /// Construct a region directly from pixel coordinates.
    #[must_use]
    pub const fn from_pixels(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// The pixel size of an overlay box to be placed inside a [`Region`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoxSize {
    /// Box width in pixels.
    pub width: f32,
    /// Box height in pixels.
    pub height: f32,
}

impl BoxSize {
    /// Construct a box size.
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

impl Default for BoxSize {
    fn default() -> Self {
        Self::new(0.0, 0.0)
    }
}

/// Per-edge inset, in pixels, applied between a [`Region`] edge and the placed
/// box. Which edges matter depends on the [`Anchor`] (a centred axis ignores its
/// padding on that axis).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Padding {
    /// Inset from the region's top edge.
    pub top: f32,
    /// Inset from the region's right edge.
    pub right: f32,
    /// Inset from the region's bottom edge.
    pub bottom: f32,
    /// Inset from the region's left edge.
    pub left: f32,
}

impl Padding {
    /// The same inset on all four edges.
    #[must_use]
    pub const fn uniform(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// A 9-point anchor describing where a box pins inside its [`Region`].
///
/// The horizontal component selects left / centre / right; the vertical
/// component selects top / centre / bottom. A centred component ignores padding
/// on that axis (you cannot offset a centred box with an edge inset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Anchor {
    /// Pin to the top-left corner.
    #[default]
    TopLeft,
    /// Centre horizontally, pin to the top edge.
    TopCenter,
    /// Pin to the top-right corner.
    TopRight,
    /// Pin to the left edge, centre vertically.
    CenterLeft,
    /// Centre on both axes.
    Center,
    /// Pin to the right edge, centre vertically.
    CenterRight,
    /// Pin to the bottom-left corner.
    BottomLeft,
    /// Centre horizontally, pin to the bottom edge.
    BottomCenter,
    /// Pin to the bottom-right corner.
    BottomRight,
}

/// One axis of an [`Anchor`]: near edge, centre, or far edge.
#[derive(Clone, Copy)]
enum Align {
    Near,
    Center,
    Far,
}

impl Align {
    /// Place a box of `extent` inside a `span` (`[origin, origin + length]`),
    /// applying the relevant edge `pad`. The near edge uses `near_pad`, the far
    /// edge uses `far_pad`, and the centre ignores padding.
    ///
    /// The result is clamped to `>= origin` so an oversized box (or aggressive
    /// padding) never escapes the region on the near edge.
    fn place(self, origin: f32, length: f32, extent: f32, near_pad: f32, far_pad: f32) -> f32 {
        let raw = match self {
            Self::Near => origin + near_pad,
            Self::Center => origin + (length - extent) / 2.0,
            Self::Far => origin + length - far_pad - extent,
        };
        raw.max(origin)
    }
}

impl Anchor {
    /// The horizontal alignment of this anchor.
    const fn horizontal(self) -> Align {
        match self {
            Self::TopLeft | Self::CenterLeft | Self::BottomLeft => Align::Near,
            Self::TopCenter | Self::Center | Self::BottomCenter => Align::Center,
            Self::TopRight | Self::CenterRight | Self::BottomRight => Align::Far,
        }
    }

    /// The vertical alignment of this anchor.
    const fn vertical(self) -> Align {
        match self {
            Self::TopLeft | Self::TopCenter | Self::TopRight => Align::Near,
            Self::CenterLeft | Self::Center | Self::CenterRight => Align::Center,
            Self::BottomLeft | Self::BottomCenter | Self::BottomRight => Align::Far,
        }
    }

    /// Place a `size` box inside `region` with `padding`, returning the
    /// pixel-space [`PixelRect`] for the compositor.
    ///
    /// Pure and total: any finite inputs yield a finite rect whose origin is
    /// pinned no earlier than the region origin (an oversized box clamps rather
    /// than overflowing the near edge).
    #[must_use]
    pub fn place(self, region: Region, size: BoxSize, padding: Padding) -> PixelRect {
        let x = self.horizontal().place(
            region.x,
            region.width,
            size.width,
            padding.left,
            padding.right,
        );
        let y = self.vertical().place(
            region.y,
            region.height,
            size.height,
            padding.top,
            padding.bottom,
        );
        PixelRect {
            x,
            y,
            width: size.width,
            height: size.height,
        }
    }
}
