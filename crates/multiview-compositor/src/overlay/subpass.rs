//! The overlay compositing **sub-pass**: blend overlay glyph quads and analytic
//! vector primitives premultiplied-source-over into the `Rgba16Float` **linear**
//! canvas, between the composite pass and the NV12 encode pass (ADR-0016 §4.1,
//! overlay-rendering.md §4.1, invariants #5 + #8).
//!
//! This module is the **CPU reference** half plus the shared, GPU-free model the
//! GPU compute path consumes ([`crate::overlay::gpu_subpass`], feature
//! `overlay` + `wgpu`). Both blend the **same** primitives into the same linear
//! premultiplied accumulator with [`crate::blend::over`], so GPU ≈ CPU
//! (overlay-rendering.md §4.4 / T7).
//!
//! # The model
//!
//! Every overlay reduces to one of a small set of premultiplied-**linear**
//! primitives ([`OverlayPrimitive`]):
//!
//! - [`OverlayPrimitive::Glyph`] — a swash coverage bitmap (straight 8-bit
//!   coverage in its alpha channel; see [`crate::overlay::text`]) tinted with a
//!   **linear** [`OverlayColor`] and blended `over` at a dest pixel. Text.
//! - [`OverlayPrimitive::FilledRect`] — a solid (optionally rounded) rectangle:
//!   alert-card chrome, tally-border fills, bar-fill meters, IDENTIFY flash.
//! - [`OverlayPrimitive::Line`] — an axis-aligned hairline/border of a given
//!   thickness: safe-area boxes, center-cross arms, tally-border edges.
//!
//! All colors are **linear**, premultiplied at blend time (swash coverage is
//! straight; ADR-R008 / invariant #8). The CPU canvas is a dense buffer of
//! premultiplied-linear `[f32; 4]` pixels ([`LinearCanvasBuffer`]); the GPU path
//! runs the identical math over the `Rgba16Float` storage texture.
//!
//! Batching (T5): [`OverlayDrawList`] holds **all** the frame's primitives in
//! one back-to-front list; the sub-pass walks it once, so the cost is constant
//! in the tile count.

use crate::blend::{over, LinearRgba, PremulRgba};
use crate::error::Result;
use crate::overlay::text::{RasterizedGlyph, RasterizedRun};
use crate::pipeline::{
    canvas_linear_to_output_yuv, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image,
};

/// A straight (non-premultiplied) **linear-light** RGBA overlay color in the
/// canvas working space, channels in `0.0..=1.0`.
///
/// Overlay colors authored in sRGB must already be linearized into the canvas
/// gamut by the caller (overlay-rendering.md §4.1); this type *is* that linear
/// value. It is premultiplied at blend time (swash coverage is straight).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayColor {
    /// Linear red, `0.0..=1.0`.
    pub r: f32,
    /// Linear green, `0.0..=1.0`.
    pub g: f32,
    /// Linear blue, `0.0..=1.0`.
    pub b: f32,
    /// Straight alpha, `0.0..=1.0`.
    pub a: f32,
}

impl OverlayColor {
    /// An opaque linear overlay color.
    #[must_use]
    pub const fn opaque(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// A linear overlay color with an explicit straight alpha.
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// The straight-alpha [`LinearRgba`] this color denotes.
    #[must_use]
    pub const fn linear_rgba(self) -> LinearRgba {
        LinearRgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a,
        }
    }
}

/// An axis-aligned pixel rectangle (top-left origin), in integer pixels.
///
/// Sub-pixel placement is a GPU-fast-path nicety (ADR-R008); the CPU oracle
/// rasterizes on the integer grid so its result is exactly assertable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayRect {
    /// Left edge, pixels.
    pub x: i32,
    /// Top edge, pixels.
    pub y: i32,
    /// Width, pixels.
    pub width: u32,
    /// Height, pixels.
    pub height: u32,
}

impl OverlayRect {
    /// Construct a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One overlay primitive in premultiplied-linear space, ready to blend `over`
/// the canvas. The CPU reference and the GPU sub-pass both consume these.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OverlayPrimitive {
    /// A shaped glyph: a swash coverage bitmap tinted by a linear color and
    /// blended at `dest`. `coverage` is the **premultiplied-RGBA8** bitmap from
    /// [`RasterizedGlyph`]; only its alpha channel (straight coverage) is used,
    /// the linear `color` supplies the tint (so HDR programs keep correct
    /// overlay color, overlay-rendering.md §4.1).
    Glyph {
        /// Top-left destination of the coverage box, in canvas pixels.
        dest_x: i32,
        /// Top-left destination of the coverage box, in canvas pixels.
        dest_y: i32,
        /// Coverage box width, pixels.
        width: u32,
        /// Coverage box height, pixels.
        height: u32,
        /// Per-pixel straight coverage, row-major (`width * height` bytes). This
        /// is the alpha channel of the swash premultiplied-RGBA8 bitmap.
        coverage: Vec<u8>,
        /// Linear tint applied to the coverage.
        color: OverlayColor,
    },
    /// A solid (optionally rounded) filled rectangle. `corner_radius` of `0`
    /// is a plain rectangle (alert-card chrome, bar-fill meters, tally fills,
    /// IDENTIFY flash).
    FilledRect {
        /// The rectangle, integer pixels.
        rect: OverlayRect,
        /// Corner radius in pixels (`0` = square corners). Rounded corners use a
        /// closed-form coverage falloff identical on CPU and GPU.
        corner_radius: u32,
        /// Linear fill color.
        color: OverlayColor,
    },
    /// An axis-aligned line / border stroke of a given thickness, drawn as a
    /// rectangle (safe-area edges, center-cross arms, tally borders).
    Line {
        /// The stroke rectangle, integer pixels (thickness is its short side).
        rect: OverlayRect,
        /// Linear stroke color.
        color: OverlayColor,
    },
    /// A thick, **angled** anti-aliased line segment (a "capsule": a segment with
    /// round end-caps), used for analog clock hands and any non-axis-aligned
    /// stroke. Coverage is the closed-form signed distance from the pixel center
    /// to the segment — identical on CPU and the GPU SDF, with a 1px linear
    /// antialias falloff at the edge (the same falloff as the rounded-rect arc).
    Stroke {
        /// First endpoint x, in **sub-pixel** canvas coordinates.
        x0: f32,
        /// First endpoint y, in sub-pixel canvas coordinates.
        y0: f32,
        /// Second endpoint x, in sub-pixel canvas coordinates.
        x1: f32,
        /// Second endpoint y, in sub-pixel canvas coordinates.
        y1: f32,
        /// Half the stroke thickness, in pixels (the capsule radius). The drawn
        /// width is `2 * half_thickness`.
        half_thickness: f32,
        /// Linear stroke color.
        color: OverlayColor,
    },
    /// A **premultiplied-RGBA bitmap** blitted (nearest-neighbour scaled) into a
    /// destination rectangle: a DVB-sub / bitmap caption cue burned into a tile.
    ///
    /// `rgba` is `src_width * src_height * 4` bytes, row-major, **already
    /// premultiplied** (this is exactly the shape the DVB-sub decoder emits,
    /// `multiview_ffmpeg::caption::CueBitmap`) — so the blend builds a
    /// [`PremulRgba`] *directly* and must **not** premultiply again. `alpha`
    /// (`0.0..=1.0`) is a uniform fade applied channel-wise to the already-
    /// premultiplied channels (a layer-opacity knob; `1.0` = no fade).
    ///
    /// The CPU reference ([`blend_image`]) is what the CLI bake uses; the GPU
    /// image-texture upload is deferred (the WGSL has a transparent no-op branch
    /// so `naga` still validates).
    Image {
        /// Destination rectangle on the canvas, integer pixels. The source is
        /// nearest-neighbour scaled to fill it.
        dest: OverlayRect,
        /// Source bitmap width in pixels.
        src_width: u32,
        /// Source bitmap height in pixels.
        src_height: u32,
        /// Premultiplied-RGBA source pixels, row-major, tightly packed (no row
        /// padding): exactly `src_width * src_height * 4` bytes.
        rgba: Vec<u8>,
        /// Uniform layer opacity in `0.0..=1.0`, applied channel-wise to the
        /// already-premultiplied source. `1.0` leaves the cue unchanged.
        alpha: f32,
    },
    /// A stroked **ring** (annulus): an anti-aliased circle outline of a given
    /// `thickness` centred at `(cx, cy)`, used for the clock bezel / tick ring.
    /// Coverage is the closed-form distance of the pixel center to the ring's
    /// mid-radius circle, with a 1px linear antialias falloff — identical CPU/GPU.
    Ring {
        /// Centre x, in sub-pixel canvas coordinates.
        cx: f32,
        /// Centre y, in sub-pixel canvas coordinates.
        cy: f32,
        /// The outer radius of the ring, in pixels.
        outer_radius: f32,
        /// The radial thickness of the ring band, in pixels (inner radius is
        /// `outer_radius - thickness`).
        thickness: f32,
        /// Linear stroke color.
        color: OverlayColor,
    },
}

impl OverlayPrimitive {
    /// Build a glyph primitive from a stage-1 [`RasterizedGlyph`], tinting its
    /// straight coverage with a linear overlay `color`.
    ///
    /// The glyph's own premultiplied RGB is discarded; only its alpha (coverage)
    /// is kept, so the same swash bitmap renders in any linear color without
    /// re-rasterizing (overlay-rendering.md §4.1 / T2).
    #[must_use]
    pub fn glyph(glyph: &RasterizedGlyph, color: OverlayColor) -> Self {
        let coverage = glyph
            .premultiplied_rgba
            .chunks_exact(4)
            .filter_map(|px| px.get(3).copied())
            .collect();
        Self::Glyph {
            dest_x: glyph.dest_x,
            dest_y: glyph.dest_y,
            width: glyph.width,
            height: glyph.height,
            coverage,
            color,
        }
    }
}

/// A horizontal bar-fill meter (PPM/VU/peak), the canonical analytic primitive
/// (overlay-rendering.md §4.2: "meters are geometry, not pictures").
///
/// `fill` in `0.0..=1.0` fills the track left→right (or, when `vertical`,
/// bottom→up) into one [`OverlayPrimitive::FilledRect`]. No bitmap, no upload.
#[must_use]
pub fn meter_bar(
    track: OverlayRect,
    fill: f32,
    vertical: bool,
    color: OverlayColor,
) -> OverlayPrimitive {
    let frac = fill.clamp(0.0, 1.0);
    if vertical {
        let filled = scale_u32(track.height, frac);
        let top = track
            .y
            .saturating_add(i32_from_u32(track.height.saturating_sub(filled)));
        OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(track.x, top, track.width, filled),
            corner_radius: 0,
            color,
        }
    } else {
        let filled = scale_u32(track.width, frac);
        OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(track.x, track.y, filled, track.height),
            corner_radius: 0,
            color,
        }
    }
}

/// Analog clock-face hand angles, in **degrees clockwise from 12 o'clock**
/// (straight up = `0°`, 3 o'clock = `90°`, 6 o'clock = `180°`).
///
/// This is the compositor-side mirror of `multiview-overlay`'s `AnalogHands` (the
/// clock *model* owns the time→angle math; this crate carries plain numbers so
/// it stays overlay-free, exactly as [`meter_bar`] takes plain dBFS deflection).
/// The CLI bridges the two ([`crate::overlay`] consumers map their model angles
/// here).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HandAngles {
    /// Hour-hand angle in degrees clockwise from 12 o'clock.
    pub hour_deg: f32,
    /// Minute-hand angle in degrees clockwise from 12 o'clock.
    pub minute_deg: f32,
    /// Second-hand angle in degrees clockwise from 12 o'clock.
    pub second_deg: f32,
}

/// The placement + sizing of an analog clock face: the centre, the bezel radius,
/// and the (derived) hand lengths / thicknesses. Hand lengths are fractions of
/// the bezel radius so the hour hand is short + thick, the minute hand longer +
/// thinner, and the second hand longest + thinnest (broadcast convention).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockFaceStyle {
    /// Centre x of the face, sub-pixel canvas coordinates.
    pub cx: f32,
    /// Centre y of the face, sub-pixel canvas coordinates.
    pub cy: f32,
    /// The bezel (outer) radius in pixels; hands + ticks scale from this.
    pub radius: f32,
}

impl ClockFaceStyle {
    /// A face centred in a `width × height` region with bezel `radius` (px).
    #[must_use]
    pub fn centred(width: u32, height: u32, radius: u32) -> Self {
        Self {
            cx: unit_dim(width) / 2.0,
            cy: unit_dim(height) / 2.0,
            radius: unit_dim(radius),
        }
    }

    /// A face at an explicit centre + bezel radius (px).
    #[must_use]
    pub const fn at(cx: f32, cy: f32, radius: f32) -> Self {
        Self { cx, cy, radius }
    }
}

/// Build the draw primitives for an analog clock face from its hand `angles` and
/// `style`: a bezel [`OverlayPrimitive::Ring`], 12 hour-tick [`OverlayPrimitive::Stroke`]s
/// around the rim, three angled hands ([`OverlayPrimitive::Stroke`]) of distinct
/// length/thickness, and a centre hub ([`OverlayPrimitive::FilledRect`], a
/// round dot). Back-to-front order: bezel, ticks, hour, minute, second, hub.
///
/// Angles are degrees clockwise from 12 o'clock (see [`HandAngles`]); `0°` is
/// straight up, increasing clockwise — so `90°` points right (3 o'clock).
#[must_use]
pub fn clock_face(
    angles: HandAngles,
    style: ClockFaceStyle,
    hour_ticks: u32,
) -> Vec<OverlayPrimitive> {
    let radius = style.radius.max(1.0);
    let color = OverlayColor::opaque(0.95, 0.95, 0.95);
    let ticks = hour_ticks.max(1);
    let mut out = Vec::with_capacity(usize::try_from(ticks).unwrap_or(24).saturating_add(5));

    // The bezel ring (outer rim). Thickness scales with the radius.
    let bezel_thick = (radius * 0.06).max(1.5);
    out.push(OverlayPrimitive::Ring {
        cx: style.cx,
        cy: style.cy,
        outer_radius: radius,
        thickness: bezel_thick,
        color,
    });

    // `hour_ticks` hour ticks (12 for a 12-hour dial, 24 for a 24-hour dial):
    // short radial strokes just inside the bezel, evenly spaced around the rim.
    let tick_outer = radius - bezel_thick;
    let tick_inner = tick_outer - (radius * 0.10).max(2.0);
    let tick_half = (radius * 0.025).max(1.0);
    let tick_step = 360.0 / unit_dim(ticks);
    for hour in 0..ticks {
        let deg = unit_dim(hour) * tick_step;
        let (ux, uy) = unit_vector(deg);
        out.push(OverlayPrimitive::Stroke {
            x0: style.cx + ux * tick_inner,
            y0: style.cy + uy * tick_inner,
            x1: style.cx + ux * tick_outer,
            y1: style.cy + uy * tick_outer,
            half_thickness: tick_half,
            color,
        });
    }

    // The three hands: distinct lengths + thicknesses (hour short+thick …
    // second long+thin), drawn from the centre.
    out.push(hand_stroke(
        style,
        angles.hour_deg,
        radius * 0.50,
        (radius * 0.045).max(2.0),
        color,
    ));
    out.push(hand_stroke(
        style,
        angles.minute_deg,
        radius * 0.72,
        (radius * 0.030).max(1.5),
        color,
    ));
    out.push(hand_stroke(
        style,
        angles.second_deg,
        radius * 0.85,
        (radius * 0.015).max(1.0),
        OverlayColor::opaque(0.95, 0.25, 0.18),
    ));

    // The centre hub: a small round dot over the hand pivot.
    let hub = (radius * 0.06).max(2.0);
    let hub_diameter = round_to_u32(hub * 2.0).max(1);
    let hub_left = round_to_i32_signed(style.cx - hub);
    let hub_top = round_to_i32_signed(style.cy - hub);
    out.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(hub_left, hub_top, hub_diameter, hub_diameter),
        corner_radius: hub_diameter / 2,
        color,
    });

    out
}

/// Build one hand [`OverlayPrimitive::Stroke`] from the centre at `deg` degrees
/// clockwise from 12 o'clock, of pixel `length` and `half_thickness`.
fn hand_stroke(
    style: ClockFaceStyle,
    deg: f32,
    length: f32,
    half_thickness: f32,
    color: OverlayColor,
) -> OverlayPrimitive {
    let (ux, uy) = unit_vector(deg);
    OverlayPrimitive::Stroke {
        x0: style.cx,
        y0: style.cy,
        x1: style.cx + ux * length,
        y1: style.cy + uy * length,
        half_thickness,
        color,
    }
}

/// The unit direction vector for `deg` degrees **clockwise from 12 o'clock**
/// (straight up). Screen y is downward, so up is `-y`: `(sin θ, -cos θ)`.
fn unit_vector(deg: f32) -> (f32, f32) {
    let rad = deg.to_radians();
    (rad.sin(), -rad.cos())
}

/// Round a non-negative `f32` to `u32` (saturating), no `as` cast.
fn round_to_u32(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let target = value.round();
    let mut lo = 0_u32;
    let mut hi = u32::MAX;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if unit_dim(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Round a (possibly negative) `f32` to `i32` (saturating), no `as` cast.
fn round_to_i32_signed(value: f32) -> i32 {
    if value < 0.0 {
        i32_from_u32(round_to_u32(-value)).saturating_neg()
    } else {
        i32_from_u32(round_to_u32(value))
    }
}

/// The full, ordered overlay primitive list for one frame, back-to-front (index
/// `0` is drawn first / furthest back). One gather per frame (T5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayDrawList {
    /// The primitives, in draw (back-to-front) order.
    pub primitives: Vec<OverlayPrimitive>,
}

impl OverlayDrawList {
    /// An empty draw list.
    #[must_use]
    pub fn new() -> Self {
        Self {
            primitives: Vec::new(),
        }
    }

    /// Append one primitive (back-to-front).
    pub fn push(&mut self, primitive: OverlayPrimitive) {
        self.primitives.push(primitive);
    }

    /// Append every glyph of a rasterized text run, tinted with `color`.
    pub fn push_run(&mut self, run: &RasterizedRun, color: OverlayColor) {
        for glyph in run.glyphs() {
            self.primitives.push(OverlayPrimitive::glyph(glyph, color));
        }
    }

    /// Number of primitives queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.primitives.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

/// A dense CPU canvas of **premultiplied-linear** RGBA `[f32; 4]` pixels.
///
/// This is the CPU mirror of the GPU `Rgba16Float` linear canvas: the composite
/// pass would seed it (premultiplied), the overlay sub-pass blends `over` it,
/// and the encode pass reads it back. The overlay sub-pass only ever *reads then
/// writes* existing pixels — it never reallocates (T3 zero per-frame alloc on
/// the GPU; the CPU oracle is a test/reference path).
#[derive(Debug, Clone, PartialEq)]
pub struct LinearCanvasBuffer {
    width: u32,
    height: u32,
    /// Row-major premultiplied-linear RGBA, `width * height` pixels.
    pixels: Vec<[f32; 4]>,
}

impl LinearCanvasBuffer {
    /// A fully transparent canvas of `width × height` premultiplied pixels.
    #[must_use]
    pub fn transparent(width: u32, height: u32) -> Self {
        let count = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().map(|h| w.saturating_mul(h)))
            .unwrap_or(0);
        Self {
            width,
            height,
            pixels: vec![[0.0; 4]; count],
        }
    }

    /// A canvas filled with one premultiplied-linear background color.
    #[must_use]
    pub fn filled(width: u32, height: u32, background: PremulRgba) -> Self {
        let mut canvas = Self::transparent(width, height);
        let bg = [background.r, background.g, background.b, background.a];
        for px in &mut canvas.pixels {
            *px = bg;
        }
        canvas
    }

    /// Canvas width, pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height, pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The backing pixel buffer's heap **capacity**, in pixels — a stable
    /// fingerprint of the allocation that holds the canvas.
    ///
    /// The overlay sub-pass blends **in place** (it only ever reads-then-writes
    /// existing pixels; see [`blend_overlays`]), never
    /// pushing or growing, so a steady-state frame must **never** reallocate this
    /// buffer (T3: zero per-frame heap allocation, ADR-0016). A reallocation would
    /// change the `Vec`'s capacity; an efficiency benchmark asserts the capacity is
    /// unchanged across a blend. (`Vec` never shrinks capacity on its own, so an
    /// unchanged capacity across the write-only blend means no allocation churn on
    /// the output buffer.)
    #[must_use]
    pub fn buffer_capacity(&self) -> usize {
        self.pixels.capacity()
    }

    /// The premultiplied-linear pixel at `(x, y)`, or `None` if out of bounds.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<PremulRgba> {
        let idx = self.index(x, y)?;
        self.pixels.get(idx).map(|p| PremulRgba {
            r: p[0],
            g: p[1],
            b: p[2],
            a: p[3],
        })
    }

    /// Linear index of `(x, y)` within bounds.
    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let w = usize::try_from(self.width).ok()?;
        let xi = usize::try_from(x).ok()?;
        let yi = usize::try_from(y).ok()?;
        yi.checked_mul(w)?.checked_add(xi)
    }

    /// Blend a premultiplied-linear `src` `over` the pixel at `(x, y)` (clipped).
    fn blend_at(&mut self, x: u32, y: u32, src: PremulRgba) {
        let Some(idx) = self.index(x, y) else {
            return;
        };
        let Some(slot) = self.pixels.get_mut(idx) else {
            return;
        };
        let dst = PremulRgba {
            r: slot[0],
            g: slot[1],
            b: slot[2],
            a: slot[3],
        };
        let out = over(src, dst);
        *slot = [out.r, out.g, out.b, out.a];
    }
}

/// Blend every primitive of `list` premultiplied-source-over the linear
/// `canvas`, in back-to-front order — the **CPU reference** overlay sub-pass.
///
/// This is the oracle the GPU compute path
/// ([`crate::overlay::gpu_subpass`]) is validated against: identical primitives,
/// identical [`over`] math, same premultiplied-linear accumulator.
pub fn blend_overlays(canvas: &mut LinearCanvasBuffer, list: &OverlayDrawList) {
    for primitive in &list.primitives {
        blend_primitive(canvas, primitive);
    }
}

/// Blend one primitive into the canvas.
fn blend_primitive(canvas: &mut LinearCanvasBuffer, primitive: &OverlayPrimitive) {
    match primitive {
        OverlayPrimitive::Glyph {
            dest_x,
            dest_y,
            width,
            height,
            coverage,
            color,
        } => blend_glyph(canvas, *dest_x, *dest_y, *width, *height, coverage, *color),
        OverlayPrimitive::FilledRect {
            rect,
            corner_radius,
            color,
        } => blend_filled_rect(canvas, *rect, *corner_radius, *color),
        OverlayPrimitive::Line { rect, color } => {
            // A line/border is a filled rectangle with square corners.
            blend_filled_rect(canvas, *rect, 0, *color);
        }
        OverlayPrimitive::Stroke {
            x0,
            y0,
            x1,
            y1,
            half_thickness,
            color,
        } => blend_stroke(canvas, *x0, *y0, *x1, *y1, *half_thickness, *color),
        OverlayPrimitive::Ring {
            cx,
            cy,
            outer_radius,
            thickness,
            color,
        } => blend_ring(canvas, *cx, *cy, *outer_radius, *thickness, *color),
        OverlayPrimitive::Image {
            dest,
            src_width,
            src_height,
            rgba,
            alpha,
        } => blend_image(canvas, *dest, *src_width, *src_height, rgba, *alpha),
    }
}

/// Blend a **premultiplied-RGBA** source bitmap, nearest-neighbour scaled, into
/// the `dest` rectangle — the bitmap-caption burn-in (DVB-sub cues).
///
/// The source `rgba` is **already premultiplied** (the DVB-sub decoder emits
/// premultiplied pixels, `multiview_ffmpeg::caption::CueBitmap`), so each sample is
/// loaded straight into a [`PremulRgba`] and blended `over` the canvas with the
/// shared [`over`] operator — it is **never** premultiplied again here. The
/// uniform `alpha` (`0.0..=1.0`) scales the already-premultiplied channels,
/// which is the correct way to fade a premultiplied layer.
///
/// The pass walks **only** the `dest` bounding box (like [`blend_stroke`]), so
/// the cost is proportional to the cue footprint, never the whole canvas.
///
/// Color note: the cue channels are sRGB-ish premultiplied while the canvas is
/// premultiplied-**linear**; for this Phase-2 MVP they are treated as already
/// being in the working space and blended directly (a Phase-2.x color
/// refinement will linearize the cue first).
fn blend_image(
    canvas: &mut LinearCanvasBuffer,
    dest: OverlayRect,
    src_width: u32,
    src_height: u32,
    rgba: &[u8],
    alpha: f32,
) {
    if dest.width == 0 || dest.height == 0 || src_width == 0 || src_height == 0 || alpha <= 0.0 {
        return;
    }
    // Guard the buffer length: a short/mismatched buffer is dropped rather than
    // indexed past (hot-path rule: never panic on the data plane).
    let expected = usize::try_from(src_width)
        .ok()
        .and_then(|w| {
            usize::try_from(src_height)
                .ok()
                .map(|h| w.saturating_mul(h))
        })
        .and_then(|px| px.checked_mul(4));
    if expected != Some(rgba.len()) {
        return;
    }
    let fade = alpha.clamp(0.0, 1.0);
    for row in 0..dest.height {
        for col in 0..dest.width {
            // Nearest-neighbour map dest (col,row) -> source (sx,sy).
            let sx = nearest(col, dest.width, src_width);
            let sy = nearest(row, dest.height, src_height);
            let Some(idx) = pixel_index(src_width, sx, sy) else {
                continue;
            };
            let base = idx.saturating_mul(4);
            // One disjoint 4-byte slice (a full RGBA quad; no indexing op).
            let Some([pr, pg, pb, pa]) = rgba.get(base..base.saturating_add(4)) else {
                continue;
            };
            // The bytes are ALREADY premultiplied — load them directly into a
            // PremulRgba (do NOT call .premultiplied()). Apply the uniform fade
            // channel-wise (correct for a premultiplied layer).
            let src = PremulRgba {
                r: unit_from_u8(*pr) * fade,
                g: unit_from_u8(*pg) * fade,
                b: unit_from_u8(*pb) * fade,
                a: unit_from_u8(*pa) * fade,
            };
            if src.a <= 0.0 {
                continue;
            }
            let Some((cx, cy)) = canvas_xy(dest.x, dest.y, col, row) else {
                continue;
            };
            canvas.blend_at(cx, cy, src);
        }
    }
}

/// Nearest-neighbour source coordinate for destination column/row `d` of
/// `dst_len`, sampling a `src_len`-wide source: `floor((d + 0.5) * src/dst)`,
/// clamped to `src_len - 1`. No `as` cast.
fn nearest(d: u32, dst_len: u32, src_len: u32) -> u32 {
    if dst_len == 0 || src_len == 0 {
        return 0;
    }
    // (2*d + 1) * src_len / (2 * dst_len), all in u64 to avoid overflow.
    let num = (u64::from(d).saturating_mul(2).saturating_add(1)).saturating_mul(u64::from(src_len));
    let den = u64::from(dst_len).saturating_mul(2).max(1);
    let s = num / den;
    let max = u64::from(src_len.saturating_sub(1));
    u32::try_from(s.min(max)).unwrap_or(src_len.saturating_sub(1))
}

/// Blend a glyph coverage bitmap tinted by `color` at `(dest_x, dest_y)`.
fn blend_glyph(
    canvas: &mut LinearCanvasBuffer,
    dest_x: i32,
    dest_y: i32,
    width: u32,
    height: u32,
    coverage: &[u8],
    color: OverlayColor,
) {
    for row in 0..height {
        for col in 0..width {
            let Some(idx) = pixel_index(width, col, row) else {
                continue;
            };
            let Some(&cov) = coverage.get(idx) else {
                continue;
            };
            if cov == 0 {
                continue;
            }
            let Some((cx, cy)) = canvas_xy(dest_x, dest_y, col, row) else {
                continue;
            };
            // Straight coverage scales the straight alpha; premultiply once.
            let alpha = color.a * unit_from_u8(cov);
            let src = LinearRgba {
                r: color.r,
                g: color.g,
                b: color.b,
                a: alpha,
            }
            .premultiplied();
            canvas.blend_at(cx, cy, src);
        }
    }
}

/// Blend a filled (optionally rounded) rectangle of `color` into the canvas.
///
/// A `corner_radius` of `0` is a sharp rectangle. Rounded corners use a
/// closed-form quarter-disc coverage: a pixel center inside the inset core or
/// within `radius` of a corner center is fully covered (coverage `1.0`); the
/// boundary pixel ring gets a linear `radius - distance` antialias falloff so
/// the CPU oracle and the GPU SDF agree on edge softness.
fn blend_filled_rect(
    canvas: &mut LinearCanvasBuffer,
    rect: OverlayRect,
    corner_radius: u32,
    color: OverlayColor,
) {
    if rect.width == 0 || rect.height == 0 || color.a <= 0.0 {
        return;
    }
    let radius = clamp_radius(corner_radius, rect.width, rect.height);
    for row in 0..rect.height {
        for col in 0..rect.width {
            let coverage = rect_coverage(rect.width, rect.height, col, row, radius);
            if coverage <= 0.0 {
                continue;
            }
            let Some((cx, cy)) = canvas_xy(rect.x, rect.y, col, row) else {
                continue;
            };
            let src = LinearRgba {
                r: color.r,
                g: color.g,
                b: color.b,
                a: color.a * coverage,
            }
            .premultiplied();
            canvas.blend_at(cx, cy, src);
        }
    }
}

/// Closed-form coverage of a rounded-rect at local pixel `(col, row)` with a
/// corner `radius` (`0` ⇒ a sharp rectangle, every pixel `1.0`). Returns
/// `0.0..=1.0`. The same math runs in the GPU shader.
fn rect_coverage(width: u32, height: u32, col: u32, row: u32, radius: u32) -> f32 {
    if radius == 0 {
        return 1.0;
    }
    let r = unit_dim(radius);
    // Pixel center in local rect coordinates.
    let px = unit_dim(col) + 0.5;
    let py = unit_dim(row) + 0.5;
    let w = unit_dim(width);
    let h = unit_dim(height);
    // Distance from the pixel center into the nearest rounded corner region.
    // dx/dy measure how far the center is *inside* each corner disc's bounding
    // axis; only when both are positive are we in a corner quadrant.
    let dx = (r - px).max(px - (w - r)).max(0.0);
    let dy = (r - py).max(py - (h - r)).max(0.0);
    if dx <= 0.0 || dy <= 0.0 {
        return 1.0;
    }
    let dist = (dx * dx + dy * dy).sqrt();
    // Linear 1px antialias falloff across the corner arc.
    (r - dist + 0.5).clamp(0.0, 1.0)
}

/// Clamp a requested corner radius to at most half the shorter side.
fn clamp_radius(radius: u32, width: u32, height: u32) -> u32 {
    let half = width.min(height) / 2;
    radius.min(half)
}

/// Blend a thick, angled anti-aliased line segment (a capsule) of `color` into
/// the canvas. Coverage is the closed-form signed distance from each pixel
/// center to the segment, with a 1px linear antialias falloff at the edge — the
/// identical math the GPU SDF runs ([`crate::overlay::gpu_subpass`]).
///
/// The pass walks the segment's padded integer bounding box only, so the cost is
/// proportional to the hand's footprint, never the whole canvas.
fn blend_stroke(
    canvas: &mut LinearCanvasBuffer,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    half_thickness: f32,
    color: OverlayColor,
) {
    let half = half_thickness.max(0.0);
    if half <= 0.0 || color.a <= 0.0 {
        return;
    }
    // Pad the bounding box by the radius plus one antialias pixel.
    let pad = half + 1.0;
    let (min_x, max_x) = ordered(x0, x1);
    let (min_y, max_y) = ordered(y0, y1);
    let (lo_x, hi_x) = bbox_span(min_x - pad, max_x + pad, canvas.width);
    let (lo_y, hi_y) = bbox_span(min_y - pad, max_y + pad, canvas.height);
    for cy in lo_y..hi_y {
        for cx in lo_x..hi_x {
            let px = unit_dim(cx) + 0.5;
            let py = unit_dim(cy) + 0.5;
            let dist = point_segment_distance(px, py, x0, y0, x1, y1);
            let coverage = (half - dist + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            blend_coverage(canvas, cx, cy, color, coverage);
        }
    }
}

/// Blend a stroked ring (annulus) of `color` into the canvas. Coverage is the
/// closed-form distance of each pixel center to the ring's mid-radius circle,
/// with a 1px linear antialias falloff — the identical math the GPU SDF runs.
fn blend_ring(
    canvas: &mut LinearCanvasBuffer,
    cx: f32,
    cy: f32,
    outer_radius: f32,
    thickness: f32,
    color: OverlayColor,
) {
    let thick = thickness.max(0.0);
    if outer_radius <= 0.0 || thick <= 0.0 || color.a <= 0.0 {
        return;
    }
    let half = thick / 2.0;
    let mid_radius = (outer_radius - half).max(0.0);
    let pad = outer_radius + 1.0;
    let (lo_x, hi_x) = bbox_span(cx - pad, cx + pad, canvas.width);
    let (lo_y, hi_y) = bbox_span(cy - pad, cy + pad, canvas.height);
    for py in lo_y..hi_y {
        for px in lo_x..hi_x {
            let sx = unit_dim(px) + 0.5 - cx;
            let sy = unit_dim(py) + 0.5 - cy;
            let radial = (sx * sx + sy * sy).sqrt();
            // Distance from the band: how far past the mid-radius circle the
            // pixel sits, beyond the band half-width.
            let dist = (radial - mid_radius).abs();
            let coverage = (half - dist + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            blend_coverage(canvas, px, py, color, coverage);
        }
    }
}

/// Premultiply `color` by `coverage` and blend it `over` the canvas at `(x, y)`.
fn blend_coverage(
    canvas: &mut LinearCanvasBuffer,
    x: u32,
    y: u32,
    color: OverlayColor,
    coverage: f32,
) {
    let src = LinearRgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a * coverage.clamp(0.0, 1.0),
    }
    .premultiplied();
    canvas.blend_at(x, y, src);
}

/// The closed-form distance from point `(px, py)` to the segment `(ax,ay)–(bx,by)`.
/// (Projects the point onto the segment, clamping the parameter to `[0, 1]`.)
fn point_segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    };
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    let ex = px - cx;
    let ey = py - cy;
    (ex * ex + ey * ey).sqrt()
}

/// Sort two `f32`s into `(min, max)` (used for the bounding box of a segment).
fn ordered(a: f32, b: f32) -> (f32, f32) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Clamp a `[lo, hi]` float span (sub-pixel) to an integer pixel range
/// `[start, end)` within `dimension`, without an `as` cast. An empty/degenerate
/// span yields `start == end` (the blend loop is then a no-op).
fn bbox_span(lo: f32, hi: f32, dimension: u32) -> (u32, u32) {
    if !lo.is_finite() || !hi.is_finite() || dimension == 0 {
        return (0, 0);
    }
    let start = clamp_floor_to_u32(lo, dimension);
    // The end is exclusive; cover the pixel containing `hi` by adding one.
    let end = clamp_floor_to_u32(hi, dimension)
        .saturating_add(1)
        .min(dimension);
    (start, end.max(start))
}

/// Floor `value` to a `u32` clamped to `[0, dimension)`, no `as` cast: a bounded
/// binary search for the largest integer whose unit image does not exceed the
/// floored value.
fn clamp_floor_to_u32(value: f32, dimension: u32) -> u32 {
    if !value.is_finite() || value < 0.0 || dimension == 0 {
        return 0;
    }
    let target = value.floor();
    let mut lo = 0_u32;
    let mut hi = dimension.saturating_sub(1);
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if unit_dim(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Row-major index of local `(col, row)` in a `width`-wide bitmap.
fn pixel_index(width: u32, col: u32, row: u32) -> Option<usize> {
    let w = usize::try_from(width).ok()?;
    let c = usize::try_from(col).ok()?;
    let r = usize::try_from(row).ok()?;
    r.checked_mul(w)?.checked_add(c)
}

/// Translate a local `(col, row)` offset from a signed dest origin to a
/// non-negative canvas `(x, y)`, or `None` if it lands left/above the canvas.
fn canvas_xy(dest_x: i32, dest_y: i32, col: u32, row: u32) -> Option<(u32, u32)> {
    let cx = dest_x.checked_add(i32_from_u32(col))?;
    let cy = dest_y.checked_add(i32_from_u32(row))?;
    let x = u32::try_from(cx).ok()?;
    let y = u32::try_from(cy).ok()?;
    Some((x, y))
}

/// `u8` coverage `0..=255` as a unit float `0.0..=1.0`.
fn unit_from_u8(v: u8) -> f32 {
    f32::from(v) / 255.0
}

/// Exact small-`u32` to `f32` (overlay sizes are well under `2^24`), no `as`.
fn unit_dim(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Saturating `u32 -> i32` (overlay coordinates are small), no `as`.
fn i32_from_u32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Scale a `u32` length by a `0.0..=1.0` fraction, rounded, without an `as`
/// cast: binary-search the integer whose unit image is nearest `len * frac`.
fn scale_u32(len: u32, frac: f32) -> u32 {
    let target = (unit_dim(len) * frac.clamp(0.0, 1.0)).round();
    let (mut lo, mut hi) = (0_u32, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if unit_dim(mid) < target {
            lo = mid.saturating_add(1);
        } else {
            hi = mid;
        }
    }
    lo
}

/// Blend an [`OverlayDrawList`] **into an existing NV12 canvas**, running the
/// overlay sub-pass through the fixed color pipeline (invariants #5 + #8).
///
/// This is the CPU-reference wire used by the engine/CLI to *bake* the resolved
/// overlays into the composited program before encode (the GPU path does this in
/// the `Rgba16Float` storage texture, [`crate::overlay::gpu_subpass`]). For each
/// pixel it: decodes the NV12 sample to linear canvas-gamut RGB (the front half
/// of the pipeline), seeds the linear premultiplied accumulator, blends every
/// primitive `over` it ([`blend_overlays`] math), then re-encodes the result to
/// NV12 (the back half). Pixels no primitive touches are decoded+re-encoded
/// identically, so an empty list is a no-op modulo color round-trip.
///
/// It runs **off the hot path** (on collected output frames), never blocks, and
/// never reallocates the planes. `canvas` describes the NV12 image's color so
/// the round-trip uses the canvas's own primaries/transfer/matrix/range.
///
/// # Errors
///
/// Returns the compositor [`crate::error::Error`] if the image color is
/// unresolved/unsupported (the same guard the composite pass applies).
///
/// This is the **full-canvas oracle reference** (un-LUT'd, every pixel
/// round-tripped). The production bake is [`apply_overlays_to_nv12`], which is
/// region-limited + LUT'd (ADR-0023) and validated against this function within
/// |Δ|≤1. Kept public as the correctness oracle + the perf baseline.
pub fn apply_overlays_to_nv12_reference(
    image: &Nv12Image,
    list: &OverlayDrawList,
    canvas: CanvasColor,
) -> Result<Nv12Image> {
    let width = image.width();
    let height = image.height();
    // Seed a dense linear premultiplied buffer from the NV12 image (decode).
    let mut buffer = LinearCanvasBuffer::transparent(width, height);
    let color = image.color();
    for y in 0..height {
        for x in 0..width {
            let Some((y8, cb8, cr8)) = image.sample(x, y) else {
                continue;
            };
            let lin = tile_yuv_to_canvas_linear(y8, cb8, cr8, color, canvas)?;
            // The program is opaque: alpha 1.0, premultiplied trivially.
            if let Some(idx) = buffer.index(x, y) {
                if let Some(slot) = buffer.pixels.get_mut(idx) {
                    *slot = [lin[0], lin[1], lin[2], 1.0];
                }
            }
        }
    }

    // Blend the overlay primitives over the seeded canvas (the shared math).
    blend_overlays(&mut buffer, list);

    // Re-encode the linear premultiplied buffer back to NV12 (the back half).
    let (w, h) = (
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
    );
    let mut y_plane = vec![0_u8; w.saturating_mul(h)];
    let mut uv_plane = vec![0_u8; w.saturating_mul(h) / 2];
    for y in 0..height {
        for x in 0..width {
            let premul = buffer.pixel(x, y).unwrap_or(PremulRgba::TRANSPARENT);
            let straight = premul.unpremultiplied();
            let out = canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?;
            write_nv12_pixel(&mut y_plane, &mut uv_plane, w, x, y, out);
        }
    }
    Nv12Image::new(width, height, y_plane, uv_plane, canvas.output_tag())
}

/// Burn `list` into `image`, returning a new NV12 program frame — the CPU
/// overlay bake the CLI/software path uses.
///
/// **Region-limited (ADR-0023).** Only the overlay primitives' even-aligned
/// footprints (one dirty rect each — *not* a single union box, so gaps between
/// spread-out overlays stay untouched) are colour round-tripped; every pixel no
/// overlay rect covers is passed through the NV12 planes byte-identically, so the
/// per-frame cost tracks the overlay footprint, not the canvas. Inside the rects
/// the output is **byte-identical** to [`apply_overlays_to_nv12_reference`] (same
/// transcendental oracle, same raster-order chroma); outside, it is exact
/// passthrough.
///
/// Runs **off the hot path** (on collected output frames); never blocks.
///
/// # Errors
///
/// Propagates the colour-conversion errors (unresolved/unsupported colour) and
/// the [`Nv12Image::new`] geometry check, exactly as
/// [`apply_overlays_to_nv12_reference`] does.
pub fn apply_overlays_to_nv12(
    image: &Nv12Image,
    list: &OverlayDrawList,
    canvas: CanvasColor,
) -> Result<Nv12Image> {
    let width = image.width();
    let height = image.height();
    let color = image.color();

    // Passing untouched pixels through verbatim is colour-correct only when the
    // input already sits in the canvas colour space — which it does: the bake
    // input is the composited canvas. If that ever differs, the whole frame
    // needs conversion, so fall back to the full-canvas reference.
    if color != canvas.output_tag() {
        return apply_overlays_to_nv12_reference(image, list, canvas);
    }

    // Start from the input planes verbatim (exact passthrough); only the dirty
    // region is overwritten below.
    let mut y_plane = image.y_plane().to_vec();
    let mut uv_plane = image.uv_plane().to_vec();

    // One even-aligned rect per primitive footprint — NOT a single union box, so
    // the gaps between spread-out overlays (labels/meters/clock across the frame)
    // stay exact passthrough and only the real footprints are processed.
    let rects = overlay_dirty_rects(list, width, height);
    if rects.is_empty() {
        // Nothing lands on the canvas: the bake is a no-op (planes unchanged).
        return Nv12Image::new(width, height, y_plane, uv_plane, canvas.output_tag());
    }

    // Seed a linear accumulator over the dirty rects only, decoding with the SAME
    // transcendental oracle the reference uses (no LUT: the overlay footprint is
    // small, so the per-pixel oracle cost is negligible here — unlike the
    // full-canvas per-tile composite hot path that ADR-0022 must LUT — and using
    // the oracle keeps the output byte-identical to the full-canvas reference
    // inside the rects). The rest of the buffer stays transparent and is never
    // read. The rects are even-aligned (whole NV12 2×2 chroma blocks) and walked
    // in raster order, so `write_nv12_pixel`'s last-writer-wins chroma matches the
    // reference exactly; overlapping rects re-process the same value (idempotent).
    let mut buffer = LinearCanvasBuffer::transparent(width, height);
    for rect in &rects {
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let Some((y8, cb8, cr8)) = image.sample(x, y) else {
                    continue;
                };
                let lin = tile_yuv_to_canvas_linear(y8, cb8, cr8, color, canvas)?;
                if let Some(idx) = buffer.index(x, y) {
                    if let Some(slot) = buffer.pixels.get_mut(idx) {
                        *slot = [lin[0], lin[1], lin[2], 1.0];
                    }
                }
            }
        }
    }

    blend_overlays(&mut buffer, list);

    let w = usize::try_from(width).unwrap_or(0);
    for rect in &rects {
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let premul = buffer.pixel(x, y).unwrap_or(PremulRgba::TRANSPARENT);
                let straight = premul.unpremultiplied();
                let out =
                    canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?;
                write_nv12_pixel(&mut y_plane, &mut uv_plane, w, x, y, out);
            }
        }
    }

    Nv12Image::new(width, height, y_plane, uv_plane, canvas.output_tag())
}

/// An even-aligned, canvas-clamped half-open rectangle `[x0, x1) × [y0, y1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirtyRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

/// One even-aligned, canvas-clamped dirty rect **per primitive footprint** (not
/// a single union box — so gaps between spread-out overlays stay exact
/// passthrough and only the real footprints are colour-processed). Empty when
/// nothing lands on the canvas.
fn overlay_dirty_rects(list: &OverlayDrawList, width: u32, height: u32) -> Vec<DirtyRect> {
    list.primitives
        .iter()
        .filter_map(|primitive| primitive_dirty_rect(primitive, width, height))
        .collect()
}

/// The even-aligned, canvas-clamped dirty rect of one primitive, or `None` if it
/// touches nothing on-canvas.
fn primitive_dirty_rect(
    primitive: &OverlayPrimitive,
    width: u32,
    height: u32,
) -> Option<DirtyRect> {
    let (x0, y0, x1, y1) = primitive_bounds(primitive, width, height)?;
    // Snap outward to even boundaries so each NV12 2×2 chroma block is wholly
    // inside or outside — matching [`write_nv12_pixel`]'s 2×2 model so a region
    // edge never splits a chroma pair. The canvas dims are even (NV12), so
    // snapping up never exceeds them; clamp anyway for safety.
    let x0 = x0 - (x0 & 1);
    let y0 = y0 - (y0 & 1);
    let x1 = x1 + (x1 & 1);
    let y1 = y1 + (y1 & 1);
    let wq = i64::from(width);
    let hq = i64::from(height);
    let x0 = x0.clamp(0, wq);
    let y0 = y0.clamp(0, hq);
    let x1 = x1.clamp(0, wq);
    let y1 = y1.clamp(0, hq);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(DirtyRect {
        x0: u32::try_from(x0).unwrap_or(0),
        y0: u32::try_from(y0).unwrap_or(0),
        x1: u32::try_from(x1).unwrap_or(0),
        y1: u32::try_from(y1).unwrap_or(0),
    })
}

/// The canvas extent (half-open, pre-even-snap, clamped to the canvas) of one
/// primitive — exactly the region its `blend_*` routine walks, so the union is a
/// provably-conservative cover. `None` if it touches nothing on-canvas.
fn primitive_bounds(
    primitive: &OverlayPrimitive,
    width: u32,
    height: u32,
) -> Option<(i64, i64, i64, i64)> {
    match primitive {
        OverlayPrimitive::Glyph {
            dest_x,
            dest_y,
            width: gw,
            height: gh,
            ..
        } => rect_bounds(
            i64::from(*dest_x),
            i64::from(*dest_y),
            *gw,
            *gh,
            width,
            height,
        ),
        OverlayPrimitive::FilledRect { rect, .. } | OverlayPrimitive::Line { rect, .. } => {
            rect_bounds(
                i64::from(rect.x),
                i64::from(rect.y),
                rect.width,
                rect.height,
                width,
                height,
            )
        }
        OverlayPrimitive::Image { dest, .. } => rect_bounds(
            i64::from(dest.x),
            i64::from(dest.y),
            dest.width,
            dest.height,
            width,
            height,
        ),
        OverlayPrimitive::Stroke {
            x0,
            y0,
            x1,
            y1,
            half_thickness,
            ..
        } => {
            let half = half_thickness.max(0.0);
            if half <= 0.0 {
                return None;
            }
            let pad = half + 1.0;
            let (min_x, max_x) = ordered(*x0, *x1);
            let (min_y, max_y) = ordered(*y0, *y1);
            span_bounds(
                min_x - pad,
                max_x + pad,
                min_y - pad,
                max_y + pad,
                width,
                height,
            )
        }
        OverlayPrimitive::Ring {
            cx,
            cy,
            outer_radius,
            ..
        } => {
            if *outer_radius <= 0.0 {
                return None;
            }
            let pad = outer_radius + 1.0;
            span_bounds(cx - pad, cx + pad, cy - pad, cy + pad, width, height)
        }
    }
}

/// Canvas extent of a `dest`/coverage rectangle, clamped to the canvas exactly
/// as `canvas_xy` clips the blend.
fn rect_bounds(
    x: i64,
    y: i64,
    w: u32,
    h: u32,
    width: u32,
    height: u32,
) -> Option<(i64, i64, i64, i64)> {
    if w == 0 || h == 0 {
        return None;
    }
    let wq = i64::from(width);
    let hq = i64::from(height);
    let x0 = x.clamp(0, wq);
    let y0 = y.clamp(0, hq);
    let x1 = (x + i64::from(w)).clamp(0, wq);
    let y1 = (y + i64::from(h)).clamp(0, hq);
    if x0 >= x1 || y0 >= y1 {
        None
    } else {
        Some((x0, y0, x1, y1))
    }
}

/// Canvas extent of an antialiased span, using the same [`bbox_span`] the
/// `blend_stroke`/`blend_ring` routines walk (so the cover is exact).
fn span_bounds(
    lo_x: f32,
    hi_x: f32,
    lo_y: f32,
    hi_y: f32,
    width: u32,
    height: u32,
) -> Option<(i64, i64, i64, i64)> {
    let (sx, ex) = bbox_span(lo_x, hi_x, width);
    let (sy, ey) = bbox_span(lo_y, hi_y, height);
    if sx >= ex || sy >= ey {
        None
    } else {
        Some((i64::from(sx), i64::from(sy), i64::from(ex), i64::from(ey)))
    }
}

/// Write one output pixel's `(y, cb, cr)` into NV12 planes (chroma per pixel,
/// last-writer-wins in each 2×2 block — the reference's nearest-neighbour
/// model, matching [`crate::pipeline::composite`]).
fn write_nv12_pixel(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    w: usize,
    px: u32,
    py: u32,
    yuv: [u8; 3],
) {
    let (Ok(xi), Ok(yi)) = (usize::try_from(px), usize::try_from(py)) else {
        return;
    };
    if let Some(slot) = y_plane.get_mut(yi.saturating_mul(w).saturating_add(xi)) {
        *slot = yuv[0];
    }
    let cx = xi / 2;
    let cy = yi / 2;
    let uv_index = cy.saturating_mul(w).saturating_add(cx.saturating_mul(2));
    if let Some(slot) = uv_plane.get_mut(uv_index) {
        *slot = yuv[1];
    }
    if let Some(slot) = uv_plane.get_mut(uv_index.saturating_add(1)) {
        *slot = yuv[2];
    }
}

#[cfg(test)]
mod dirty_rects_tests {
    use super::{
        overlay_dirty_rects, DirtyRect, OverlayColor, OverlayDrawList, OverlayPrimitive,
        OverlayRect,
    };

    fn green() -> OverlayColor {
        OverlayColor::opaque(0.0, 1.0, 0.0)
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> OverlayPrimitive {
        OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(x, y, w, h),
            corner_radius: 0,
            color: green(),
        }
    }

    #[test]
    fn empty_list_has_no_dirty_rects() {
        assert!(overlay_dirty_rects(&OverlayDrawList::new(), 64, 64).is_empty());
    }

    #[test]
    fn a_single_even_rect_is_its_own_dirty_rect() {
        let mut list = OverlayDrawList::new();
        list.push(rect(8, 4, 8, 6));
        assert_eq!(
            overlay_dirty_rects(&list, 64, 64),
            vec![DirtyRect {
                x0: 8,
                y0: 4,
                x1: 16,
                y1: 10
            }]
        );
    }

    #[test]
    fn an_odd_rect_snaps_outward_to_even() {
        let mut list = OverlayDrawList::new();
        // x[5,9) y[3,7) -> even-snapped to x[4,10) y[2,8).
        list.push(rect(5, 3, 4, 4));
        assert_eq!(
            overlay_dirty_rects(&list, 64, 64),
            vec![DirtyRect {
                x0: 4,
                y0: 2,
                x1: 10,
                y1: 8
            }]
        );
    }

    #[test]
    fn two_separated_rects_stay_separate_not_unioned() {
        let mut list = OverlayDrawList::new();
        list.push(rect(2, 2, 4, 4));
        list.push(rect(20, 16, 6, 6));
        assert_eq!(
            overlay_dirty_rects(&list, 64, 64),
            vec![
                DirtyRect {
                    x0: 2,
                    y0: 2,
                    x1: 6,
                    y1: 6
                },
                DirtyRect {
                    x0: 20,
                    y0: 16,
                    x1: 26,
                    y1: 22
                },
            ]
        );
    }

    #[test]
    fn a_rect_is_clamped_to_the_canvas() {
        let mut list = OverlayDrawList::new();
        list.push(rect(60, 60, 20, 20));
        assert_eq!(
            overlay_dirty_rects(&list, 64, 64),
            vec![DirtyRect {
                x0: 60,
                y0: 60,
                x1: 64,
                y1: 64
            }]
        );
    }

    #[test]
    fn a_fully_offscreen_primitive_yields_no_rect() {
        let mut list = OverlayDrawList::new();
        list.push(rect(-40, -40, 10, 10));
        assert!(overlay_dirty_rects(&list, 64, 64).is_empty());
    }

    #[test]
    fn a_ring_rect_covers_centre_plus_radius_and_aa() {
        let mut list = OverlayDrawList::new();
        list.push(OverlayPrimitive::Ring {
            cx: 32.0,
            cy: 32.0,
            outer_radius: 10.0,
            thickness: 3.0,
            color: green(),
        });
        let rects = overlay_dirty_rects(&list, 64, 64);
        assert_eq!(rects.len(), 1, "one ring -> one rect");
        let b = rects[0];
        assert!(
            b.x0 <= 21 && b.x1 >= 43 && b.y0 <= 21 && b.y1 >= 43,
            "ring rect must cover centre±(radius+1): {b:?}"
        );
    }
}
