//! Closed-form wipe masks (ADR-0112 §3).
//!
//! A [`WipeMask`] turns one full-canvas tile into a WIPE: its coverage at a
//! canvas pixel multiplies into the tile alpha at the single premultiply step,
//! so the colour pipeline order is unchanged (invariant #8). The same formula
//! runs in `composite.wgsl` (`wipe_coverage`); both evaluate it in `f32` from
//! the same precomputed parameters, so CPU and GPU differ only at edge pixels
//! by float rounding.
//!
//! Geometry: canvas coordinates are normalised to `u = (px + 0.5) / width`,
//! `v = (py + 0.5) / height`. Each shape is a distance field `f(u, v) >= 0`
//! whose largest value on the canvas is `extent`; the revealed region is
//! `f <= radius`. `reverse` mirrors it: the region outside the shape of radius
//! `extent - radius` (the other side for an edge, closing in from the outside
//! for a box or circle). The signed distance to the edge is measured in
//! canvas-height units for every shape (the box's sides included), so a soft
//! edge or a border band has the same pixel width on every side.
//!
//! The endpoints are exact (review MED-01): progress `<= 0` covers nothing and
//! progress `>= 1` covers everything, whatever the origin or rounding.

/// The mask family (the compositor's own copy, so this crate never matches on
/// the non-exhaustive switcher descriptor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaskShape {
    /// A vertical edge sweeping left to right.
    Horizontal,
    /// A horizontal edge sweeping top to bottom.
    Vertical,
    /// A diagonal edge sweeping from the top-left corner.
    Diagonal,
    /// A canvas-aspect rectangle growing from `origin`.
    Box,
    /// A circle (iris) growing from `origin`, round on the canvas.
    Circle,
}

impl MaskShape {
    /// The id shared with `composite.wgsl` (`0` is "no mask").
    #[must_use]
    pub const fn id(self) -> u32 {
        match self {
            Self::Horizontal => 1,
            Self::Vertical => 2,
            Self::Diagonal => 3,
            Self::Box => 4,
            Self::Circle => 5,
        }
    }
}

/// A wipe mask at one progress value, with every per-pixel constant already
/// derived (no per-pixel branching on the request beyond the shape).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WipeMask {
    /// The shape.
    pub shape: MaskShape,
    /// The canvas width in pixels the mask is evaluated on.
    pub width: u32,
    /// The canvas height in pixels.
    pub height: u32,
    /// The canvas aspect `width / height` the field is evaluated at.
    pub aspect: f32,
    /// The pattern origin in normalised canvas coordinates.
    pub origin: [f32; 2],
    /// Mirror the field.
    pub reverse: bool,
    /// The field's largest value on the canvas.
    pub extent: f32,
    /// The edge position in field units: pixels with `f <= radius` are
    /// revealed.
    pub radius: f32,
    /// The soft-edge width in canvas-height units (`0` is a hard edge).
    pub softness: f32,
    /// The border band's width in canvas-height units: `> 0` makes this the
    /// band's draw: the edge grown by this much, attenuated by the edge's own
    /// coverage, so never the revealed interior (review MED-03); `0`
    /// is the incoming picture's own mask.
    pub expand: f32,
    /// An exact coverage for the whole canvas at an endpoint: `Some(0.0)` at
    /// progress `<= 0`, `Some(1.0)` at progress `>= 1`, else `None`.
    pub pinned: Option<f32>,
}

impl WipeMask {
    /// The mask of `shape` at `progress` in `[0, 1]` on a `width x height`
    /// canvas. `softness` and `border` are fractions of the canvas height
    /// (clamped to `[0, 1]`); the travel is lengthened by the soft half-width
    /// and the border so that progress `0` reveals nothing (border included)
    /// and progress `1` reveals everything.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    // reason: the mask request is exactly these eight scalars; a builder would
    // only rename them.
    pub fn new(
        shape: MaskShape,
        progress: f32,
        width: u32,
        height: u32,
        origin: [f32; 2],
        reverse: bool,
        softness: f32,
        border: f32,
    ) -> Self {
        let aspect = aspect(width, height);
        let origin = [clamp01(origin[0]), clamp01(origin[1])];
        let extent = extent(shape, aspect, origin);
        let softness = clamp01(softness);
        let border = clamp01(border);
        // The radius is in field units; the margin is in height units. A box
        // side's height-unit distance is `aspect * (radius - |du|)`, so on a
        // portrait canvas the margin is scaled up to clear both axes.
        let scale = match shape {
            MaskShape::Box => 1.0_f32.max(1.0 / aspect),
            _ => 1.0,
        };
        let margin = (softness * 0.5 + border) * scale;
        let progress = clamp01(progress);
        let radius = -margin + progress * (extent + softness * 0.5 * scale + margin);
        let pinned = if progress <= 0.0 {
            Some(0.0)
        } else if progress >= 1.0 {
            Some(1.0)
        } else {
            None
        };
        Self {
            shape,
            width: width.max(1),
            height: height.max(1),
            aspect,
            origin,
            reverse,
            extent,
            radius,
            softness,
            expand: 0.0,
            pinned,
        }
    }

    /// The border band of this edge, `expand` wide (canvas-height units):
    /// coverage of the edge grown by `expand` times one minus the edge's own
    /// coverage. The band never covers the revealed interior, so a missing,
    /// stale or transparent incoming picture shows the outgoing one there,
    /// never the border colour (review MED-03). At either endpoint the band
    /// covers nothing.
    #[must_use]
    pub fn expanded(mut self, expand: f32) -> Self {
        self.expand = clamp01(expand);
        if self.expand > 0.0 && self.pinned.is_some() {
            self.pinned = Some(0.0);
        }
        self
    }

    /// Coverage in `[0, 1]` at canvas pixel `(px, py)`. Must stay identical
    /// to `wipe_coverage` in `composite.wgsl`.
    #[must_use]
    pub fn coverage(&self, px: u32, py: u32) -> f32 {
        if let Some(pinned) = self.pinned {
            return pinned;
        }
        let u = (u32_to_f32(px) + 0.5) / u32_to_f32(self.width);
        let v = (u32_to_f32(py) + 0.5) / u32_to_f32(self.height);
        let (radius, sign) = if self.reverse {
            (self.extent - self.radius, -1.0)
        } else {
            (self.radius, 1.0)
        };
        let d = sign * inside(self.shape, self.aspect, self.origin, radius, u, v);
        if self.expand > 0.0 {
            // The grown edge attenuated by the edge itself: smooth on both
            // sides of a soft band, zero wherever the incoming picture is
            // fully revealed (review MED-03).
            edge(d + self.expand, self.softness) * (1.0 - edge(d, self.softness))
        } else {
            edge(d, self.softness)
        }
    }
}

/// Coverage of a signed distance `d` (positive inside) through an edge of
/// width `softness` (`0` is hard). Mirrors `wipe_edge` in `composite.wgsl`.
fn edge(d: f32, softness: f32) -> f32 {
    if softness > 0.0 {
        (d / softness + 0.5).clamp(0.0, 1.0)
    } else if d >= 0.0 {
        1.0
    } else {
        0.0
    }
}

fn clamp01(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0)
    }
}

/// An exact-enough `u32 -> f32` for canvas sizes (`< 2^24`).
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: canvas coordinates are below 2^24, so the conversion is exact; the
// shader performs the same `f32(u32)` conversion.
fn u32_to_f32(x: u32) -> f32 {
    x as f32
}

fn aspect(width: u32, height: u32) -> f32 {
    u32_to_f32(width.max(1)) / u32_to_f32(height.max(1))
}

const INV_SQRT2: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// The signed distance, in canvas-height units, from `(u, v)` to the edge of
/// the shape of field radius `radius` (positive inside). Must stay identical
/// to the `switch` in `wipe_coverage` (`composite.wgsl`).
fn inside(shape: MaskShape, aspect: f32, origin: [f32; 2], radius: f32, u: f32, v: f32) -> f32 {
    match shape {
        MaskShape::Horizontal => radius - u * aspect,
        MaskShape::Vertical => radius - v,
        MaskShape::Diagonal => radius - (u * aspect + v) * INV_SQRT2,
        // A canvas-aspect rectangle of normalised half-size `radius`: its
        // side distances in height units (review MED-02).
        MaskShape::Box => {
            (aspect * (radius - (u - origin[0]).abs())).min(radius - (v - origin[1]).abs())
        }
        MaskShape::Circle => {
            let dx = (u - origin[0]) * aspect;
            let dy = v - origin[1];
            radius - (dx * dx + dy * dy).sqrt()
        }
    }
}

/// The largest field value on the canvas (at a corner or edge).
fn extent(shape: MaskShape, aspect: f32, origin: [f32; 2]) -> f32 {
    match shape {
        MaskShape::Horizontal => aspect,
        MaskShape::Vertical => 1.0,
        MaskShape::Diagonal => (aspect + 1.0) * INV_SQRT2,
        MaskShape::Box => origin[0]
            .max(1.0 - origin[0])
            .max(origin[1])
            .max(1.0 - origin[1]),
        MaskShape::Circle => {
            let dx = origin[0].max(1.0 - origin[0]) * aspect;
            let dy = origin[1].max(1.0 - origin[1]);
            (dx * dx + dy * dy).sqrt()
        }
    }
}
