//! Audio **meters & scopes** as overlay draw-data (feature `overlay`, ADR-0016 /
//! overlay-rendering.md §4.2: "meters are geometry, not pictures").
//!
//! The engine taps the pure-DSP meters in `multiview-audio` (PPM/VU/peak
//! ballistics, EBU R128, stereo correlation/goniometer) read-only and off the
//! hot path; at a **conflated ~30 Hz** the latest reading is turned here into a
//! handful of analytic [`OverlayPrimitive`]s — bar-fill rectangles, peak-hold
//! ticks, goniometer dots, histogram columns — that the overlay sub-pass blends
//! into the linear canvas. No bitmap, no per-frame upload (T1/T3): a meter is
//! just a few rectangles whose extents are recomputed each conflated frame.
//!
//! All inputs are plain numbers (dBFS, unit goniometer coordinates, bin counts)
//! so this module carries **no audio or overlay dependency** — it bridges the
//! `multiview-audio` readings (mapped to a `0.0..=1.0` deflection by
//! [`crate::overlay`] callers / `multiview-audio`) into the compositor's primitive
//! model. The dB→deflection mapping and peak-hold also live here so the CPU
//! reference and any GPU path agree.

use crate::overlay::subpass::{
    meter_bar, OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect,
};

/// The dBFS window a meter scale maps onto its `0.0..=1.0` track deflection.
///
/// A reading at or below [`floor_db`](Self::floor_db) reads empty (`0.0`); at or
/// above [`ceil_db`](Self::ceil_db) reads full (`1.0`); in between it is linear
/// in dB (the conventional log meter scale — equal dB steps are equal pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterScale {
    /// dBFS at the bottom of the track (empty). Typically `-60.0`.
    floor_db: f32,
    /// dBFS at the top of the track (full). Typically `0.0`.
    ceil_db: f32,
}

impl Default for MeterScale {
    /// A `-60 dBFS … 0 dBFS` digital-peak scale — the common full-scale meter.
    fn default() -> Self {
        Self {
            floor_db: -60.0,
            ceil_db: 0.0,
        }
    }
}

impl MeterScale {
    /// Construct a scale spanning `floor_db … ceil_db` dBFS. The floor is clamped
    /// strictly below the ceiling so the mapping never divides by zero.
    #[must_use]
    pub fn new(floor_db: f32, ceil_db: f32) -> Self {
        let floor_db = floor_db.min(ceil_db - f32::EPSILON);
        Self { floor_db, ceil_db }
    }

    /// The bottom-of-track level in dBFS.
    #[must_use]
    pub const fn floor_db(self) -> f32 {
        self.floor_db
    }

    /// The top-of-track level in dBFS.
    #[must_use]
    pub const fn ceil_db(self) -> f32 {
        self.ceil_db
    }

    /// Map a `db` reading to a `0.0..=1.0` track deflection, linear in dB.
    ///
    /// `db <= floor` → `0.0`; `db >= ceil` → `1.0`. A non-finite reading reads
    /// empty (defensive: meter math is finite, but the bar must never NaN).
    #[must_use]
    pub fn deflection(self, db: f32) -> f32 {
        if !db.is_finite() {
            return 0.0;
        }
        let span = self.ceil_db - self.floor_db;
        if span <= 0.0 {
            return 0.0;
        }
        ((db - self.floor_db) / span).clamp(0.0, 1.0)
    }
}

/// A single dB meter with **peak-hold**: it tracks an instantaneous deflection
/// and the highest deflection seen since the hold last decayed, so the renderer
/// can draw a moving fill plus a held peak tick (broadcast PPM/peak convention).
///
/// This is a tiny value-machine driven by the conflated meter samples; it holds
/// no audio state and never blocks. Construct with [`MeterBar::new`], push the
/// latest reading with [`MeterBar::observe_db`], and decay the held peak with
/// [`MeterBar::decay_peak`] (called once per conflated frame).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterBar {
    scale: MeterScale,
    /// Current instantaneous deflection (`0.0..=1.0`).
    level: f32,
    /// Held peak deflection (`0.0..=1.0`), `>= level`.
    peak: f32,
}

impl MeterBar {
    /// A meter bar on `scale`, starting empty.
    #[must_use]
    pub fn new(scale: MeterScale) -> Self {
        Self {
            scale,
            level: 0.0,
            peak: 0.0,
        }
    }

    /// Record the latest meter reading (dBFS), updating the live level and
    /// raising the held peak if this reading exceeds it.
    pub fn observe_db(&mut self, db: f32) {
        self.level = self.scale.deflection(db);
        if self.level > self.peak {
            self.peak = self.level;
        }
    }

    /// Decay the held peak toward the current level by `amount` deflection units
    /// (clamped so it never drops below the live level). Called once per
    /// conflated frame so the peak tick falls back gradually.
    pub fn decay_peak(&mut self, amount: f32) {
        let decayed = self.peak - amount.max(0.0);
        self.peak = decayed.max(self.level);
    }

    /// The current live deflection (`0.0..=1.0`).
    #[must_use]
    pub const fn level(self) -> f32 {
        self.level
    }

    /// The held-peak deflection (`0.0..=1.0`).
    #[must_use]
    pub const fn peak(self) -> f32 {
        self.peak
    }

    /// Build the draw primitives for this meter inside `track`: a dim background,
    /// the live fill, and a bright peak-hold tick. Vertical meters fill
    /// bottom→up; horizontal meters fill left→right.
    ///
    /// `fill` colours the live bar; `peak` colours the hold tick; the track
    /// background is `fill` at a low alpha so the empty headroom still reads.
    #[must_use]
    pub fn primitives(
        self,
        track: OverlayRect,
        vertical: bool,
        fill: OverlayColor,
        peak: OverlayColor,
    ) -> Vec<OverlayPrimitive> {
        let mut out = Vec::with_capacity(3);
        // Dim background for the full track so the headroom reads.
        out.push(OverlayPrimitive::FilledRect {
            rect: track,
            corner_radius: 0,
            color: OverlayColor::new(fill.r, fill.g, fill.b, fill.a * 0.18),
        });
        // The live fill.
        out.push(meter_bar(track, self.level, vertical, fill));
        // The peak-hold tick: a thin band at the held-peak position.
        if let Some(tick) = peak_tick(track, self.peak, vertical, peak) {
            out.push(tick);
        }
        out
    }

    /// Append this meter's primitives to a draw list (convenience over
    /// [`Self::primitives`]).
    pub fn push_into(
        self,
        list: &mut OverlayDrawList,
        track: OverlayRect,
        vertical: bool,
        fill: OverlayColor,
        peak: OverlayColor,
    ) {
        for prim in self.primitives(track, vertical, fill, peak) {
            list.push(prim);
        }
    }
}

/// A 1px-wide peak-hold tick at deflection `frac` within `track`, or [`None`] if
/// the peak is at the very bottom (nothing to draw).
fn peak_tick(
    track: OverlayRect,
    frac: f32,
    vertical: bool,
    color: OverlayColor,
) -> Option<OverlayPrimitive> {
    let frac = frac.clamp(0.0, 1.0);
    if frac <= 0.0 {
        return None;
    }
    if vertical {
        let filled = scale_u32(track.height, frac);
        let offset = track.height.saturating_sub(filled);
        let top = track.y.saturating_add(i32_from_u32(offset));
        Some(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(track.x, top, track.width, 1),
            corner_radius: 0,
            color,
        })
    } else {
        let filled = scale_u32(track.width, frac);
        // Place the 1px tick at the right edge of the filled region.
        let x = track
            .x
            .saturating_add(i32_from_u32(filled.saturating_sub(1)));
        Some(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(x, track.y, 1, track.height),
            corner_radius: 0,
            color,
        })
    }
}

/// One goniometer (Lissajous) dot in unit display space: `x` is the side
/// (stereo-width) coordinate, `y` the mid (mono) coordinate, conventionally each
/// in roughly `-1.0..=1.0` for full-scale audio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GonioDot {
    /// Side (L−R) coordinate.
    pub x: f32,
    /// Mid (L+R) coordinate.
    pub y: f32,
}

/// Render a set of goniometer dots into `box_rect`: each unit `(x, y)` maps to a
/// small square dot inside the box (centre at the box centre, `x`/`y` scaled to
/// half the box extent). Out-of-range dots are clamped to the box. `dot_px` sizes
/// each dot.
#[must_use]
pub fn goniometer(
    box_rect: OverlayRect,
    dots: &[GonioDot],
    dot_px: u32,
    color: OverlayColor,
) -> Vec<OverlayPrimitive> {
    let half_w = unit_dim(box_rect.width) / 2.0;
    let half_h = unit_dim(box_rect.height) / 2.0;
    let cx = unit_dim_signed(box_rect.x) + half_w;
    let cy = unit_dim_signed(box_rect.y) + half_h;
    let size = dot_px.max(1);
    let mut out = Vec::with_capacity(dots.len());
    for dot in dots {
        if !dot.x.is_finite() || !dot.y.is_finite() {
            continue;
        }
        let px = cx + dot.x.clamp(-1.0, 1.0) * half_w;
        let py = cy - dot.y.clamp(-1.0, 1.0) * half_h; // screen y is downward
        let x = round_to_i32(px).saturating_sub(i32_from_u32(size / 2));
        let y = round_to_i32(py).saturating_sub(i32_from_u32(size / 2));
        out.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(x, y, size, size),
            corner_radius: 0,
            color,
        });
    }
    out
}

/// Render a luma/level **histogram** as side-by-side column bars inside
/// `box_rect`. `bins` are raw counts; the tallest column fills the box height,
/// the rest scale proportionally (so the shape reads regardless of sample
/// count). Columns are laid left→right, evenly dividing the box width.
#[must_use]
pub fn histogram(
    box_rect: OverlayRect,
    bins: &[u64],
    color: OverlayColor,
) -> Vec<OverlayPrimitive> {
    if bins.is_empty() || box_rect.width == 0 || box_rect.height == 0 {
        return Vec::new();
    }
    let max = bins.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return Vec::new();
    }
    let count = u32::try_from(bins.len()).unwrap_or(u32::MAX);
    let col_w = (box_rect.width / count).max(1);
    let mut out = Vec::with_capacity(bins.len());
    for (i, &v) in bins.iter().enumerate() {
        if v == 0 {
            continue;
        }
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        let frac = ratio(v, max);
        let h = scale_u32(box_rect.height, frac);
        if h == 0 {
            continue;
        }
        let x = box_rect
            .x
            .saturating_add(i32_from_u32(idx.saturating_mul(col_w)));
        let y = box_rect
            .y
            .saturating_add(i32_from_u32(box_rect.height.saturating_sub(h)));
        out.push(OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(x, y, col_w, h),
            corner_radius: 0,
            color,
        });
    }
    out
}

/// `numer / denom` as a `0.0..=1.0` f32, exact for the small counts meters
/// produce (no `as` cast).
fn ratio(numer: u64, denom: u64) -> f32 {
    if denom == 0 {
        return 0.0;
    }
    (u64_to_f32(numer) / u64_to_f32(denom)).clamp(0.0, 1.0)
}

/// Lossless-enough `u64 -> f32` for the magnitudes meters produce (bin counts
/// well under `2^24`), avoiding an `as` cast.
fn u64_to_f32(value: u64) -> f32 {
    u32::try_from(value).map_or(f32::from(u16::MAX) * 65_536.0, unit_dim)
}

/// Exact small-`u32` to `f32` (overlay sizes are well under `2^24`), no `as`.
fn unit_dim(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Exact small-`i32` to `f32`, no `as`.
fn unit_dim_signed(value: i32) -> f32 {
    if value < 0 {
        -unit_dim(value.unsigned_abs())
    } else {
        unit_dim(u32::try_from(value).unwrap_or(u32::MAX))
    }
}

/// Saturating `u32 -> i32` (overlay sizes are small), no `as` cast.
fn i32_from_u32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Round a non-negative-ish `f32` pixel coordinate to the nearest `i32` without
/// an `as` cast: binary-search the integer whose value is nearest `value`.
fn round_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let rounded = value.round();
    // Bound the search to the representable overlay coordinate range.
    let (mut lo, mut hi): (i32, i32) = (i32::MIN / 2, i32::MAX / 2);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if i32_to_f32(mid) < rounded {
            lo = mid.saturating_add(1);
        } else {
            hi = mid;
        }
    }
    lo
}

/// Exact `i32 -> f32` for the bounded coordinate range used by [`round_to_i32`]
/// (`|value| < 2^30`, always representable), no `as` cast.
fn i32_to_f32(value: i32) -> f32 {
    unit_dim_signed(value)
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
