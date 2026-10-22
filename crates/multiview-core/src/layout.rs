//! Declarative layout/template model (canvas + cells) and its validation.
//!
//! The layered model is Canvas -> Layout -> Cells (overlays live in
//! `multiview-overlay`). A resolver in a downstream crate flattens a validated
//! [`Layout`] into draw quads for the compositor; [`Layout::validate`] enforces
//! the structural invariants that resolver relies on.
use crate::error::{Error, Result};
use crate::time::Rational;
use serde::{Deserialize, Serialize};

/// How a source is fitted into its cell rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FitMode {
    /// Scale to fit entirely inside the cell (letterbox/pillarbox).
    #[default]
    Contain,
    /// Scale to cover the cell, cropping overflow.
    Cover,
    /// Stretch to the cell, ignoring aspect ratio.
    Fill,
}

/// A quarter-turn (90°-step) rotation: the shared `none`/`cw90`/`cw180`/`cw270`
/// vocabulary used by the per-output presentation orientation (ADR-0089).
///
/// Only quarter turns are offered so a pixel rotation stays lossless and
/// GPU-cheap (a sampling transform, no resampling artefacts) and a
/// display-rotation tag can carry it exactly. `#[non_exhaustive]` for forward
/// compatibility.
///
/// Serializes in **`snake_case`** (`none`/`cw90`/`cw180`/`cw270`) — the
/// project-wide serde convention and the authoring spelling the config schema
/// documents for the per-output `orientation.turn` (ADR-0089 §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuarterTurn {
    /// No rotation (0°).
    #[default]
    None,
    /// Rotate 90° clockwise.
    Cw90,
    /// Rotate 180°.
    Cw180,
    /// Rotate 270° clockwise (90° counter-clockwise).
    Cw270,
}

impl QuarterTurn {
    /// The rotation in clockwise degrees (`0`, `90`, `180`, `270`).
    #[must_use]
    pub const fn degrees(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Cw90 => 90,
            Self::Cw180 => 180,
            Self::Cw270 => 270,
        }
    }

    /// Whether this rotation swaps the width/height axes (the odd quarter turns,
    /// 90° and 270°).
    #[must_use]
    pub const fn swaps_axes(self) -> bool {
        matches!(self, Self::Cw90 | Self::Cw270)
    }
}

/// Output orientation of a head/canvas (landscape vs portrait mounting).
///
/// Landscape is the default; portrait swaps the logical width/height of the
/// output (see [`Orientation::swaps_axes`]). `#[non_exhaustive]` for forward
/// compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Orientation {
    /// Standard landscape (no axis swap).
    #[default]
    Landscape,
    /// Portrait (logical width/height swapped).
    Portrait,
}

impl Orientation {
    /// Whether this orientation swaps the width/height axes ([`Orientation::Portrait`]).
    #[must_use]
    pub const fn swaps_axes(self) -> bool {
        matches!(self, Self::Portrait)
    }
}

/// The fully-opaque default opacity (`1.0`) for a [`Cell`].
///
/// Used as the serde default and by [`Cell`]'s [`Default`] impl so a cell built
/// without an opacity (or deserialized from a document predating the field)
/// composites as a hard-cover, exactly as before.
const fn default_opacity() -> f32 {
    1.0
}

/// One multiview cell/tile: a normalized rectangle (`0.0..=1.0`) on the canvas.
///
/// The per-tile [`opacity`](Cell::opacity) is **additive and defaulted**: a
/// `Cell` constructed without it (or deserialized from a document predating it)
/// composites as a hard-cover, exactly as before. Use `..Cell::default()` in
/// struct literals to opt out of the new field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// Left edge (fraction of canvas width).
    pub x: f32,
    /// Top edge (fraction of canvas height).
    pub y: f32,
    /// Width (fraction of canvas width).
    pub w: f32,
    /// Height (fraction of canvas height).
    pub h: f32,
    /// Stacking order (higher draws on top).
    pub z: i32,
    /// Fit mode.
    pub fit: FitMode,
    /// Bound source id, if any.
    pub source: Option<String>,
    /// Per-tile uniform opacity (straight alpha) in the closed interval
    /// `[0.0, 1.0]`, applied in the compositor's premultiplied linear-light
    /// `over` blend. Defaults to `1.0` (fully opaque / hard-cover); a lower
    /// value lets an overlapping/z-stacked tile cross-fade or PiP-ghost over the
    /// tiles beneath it. [`Layout::validate`] rejects values outside the
    /// interval (and non-finite values).
    #[serde(default = "default_opacity")]
    pub opacity: f32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            z: 0,
            fit: FitMode::default(),
            source: None,
            opacity: default_opacity(),
        }
    }
}

/// The output canvas description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Canvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Output frame-rate numerator.
    pub fps_num: i64,
    /// Output frame-rate denominator (1001 for NTSC families).
    pub fps_den: i64,
}

impl Canvas {
    /// The output cadence (frame rate) as an exact [`Rational`].
    ///
    /// The flat `fps_num`/`fps_den` fields are kept for clean, human-editable
    /// TOML; this accessor is the seam the engine output clock and config layers
    /// consume so they carry an exact rational (invariant #3 — never a float
    /// fps). The value is returned verbatim (not reduced) so `30000/1001`
    /// survives intact; callers needing canonical form call
    /// [`Rational::reduce`], and [`Layout::validate`] rejects an invalid cadence
    /// before it reaches the clock.
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        Rational::new(self.fps_num, self.fps_den)
    }

    /// Whether two canvases denote the **same output signal**: identical pixel
    /// geometry and the same cadence **by value** ([`Rational`]'s `PartialEq`
    /// cross-multiplies in `i128`), so a non-reduced `50/2` cadence equals
    /// `25/1`. The live-apply pinned-canvas gates (ADR-W019 / ADR-R004)
    /// compare with this — never with the structural derived `==`, whose
    /// `fps_num`/`fps_den` fields deliberately distinguish non-reduced forms.
    #[must_use]
    pub fn same_signal(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.cadence() == other.cadence()
    }

    /// Validate this canvas in isolation.
    ///
    /// Enforces positive pixel dimensions and a valid positive rational cadence
    /// (`fps_num > 0`, `fps_den > 0`, and [`Rational::is_valid`]). Shared by
    /// [`Layout::validate`] and [`Head::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] naming the offending field.
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 {
            return Err(Error::Config("canvas width must be > 0".to_owned()));
        }
        if self.height == 0 {
            return Err(Error::Config("canvas height must be > 0".to_owned()));
        }
        if self.fps_num <= 0 {
            return Err(Error::Config(format!(
                "canvas fps_num must be > 0 (got {})",
                self.fps_num
            )));
        }
        if self.fps_den <= 0 {
            return Err(Error::Config(format!(
                "canvas fps_den must be > 0 (got {})",
                self.fps_den
            )));
        }
        // Defensive backstop: the cadence the engine clock will consume must be
        // a usable rational. The positive-field checks above already imply this,
        // but validate the exact value the seam returns so the two can never
        // drift apart.
        if !self.cadence().is_valid() {
            return Err(Error::Config(format!(
                "canvas cadence {}/{} is not a valid rational",
                self.fps_num, self.fps_den
            )));
        }
        // The per-tick period must be at least 1 ns (i.e. fps <= 1 GHz, far above
        // any real video rate). A sub-nanosecond period collapses consecutive ticks
        // onto the same nanosecond, which breaks strictly-increasing output PTS
        // (invariant #3) and defeats the output clock's exact-floor wall-clock resync
        // (ADR-T018). Reject it here rather than silently degrade.
        if self.cadence().has_subnanosecond_period() {
            return Err(Error::Config(format!(
                "canvas cadence {}/{} is too fast: the per-tick period is under 1 ns \
                 (fps must be <= 1 GHz)",
                self.fps_num, self.fps_den
            )));
        }
        Ok(())
    }
}

/// A complete named layout/template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// Template name.
    pub name: String,
    /// Output canvas.
    pub canvas: Canvas,
    /// Cells in declaration order.
    pub cells: Vec<Cell>,
}

impl Layout {
    /// Validate the structural invariants of this layout.
    ///
    /// Enforces:
    /// - canvas `width` and `height` are both `> 0`;
    /// - the output cadence ([`Canvas::cadence`]) is a valid positive rational
    ///   (`fps_num > 0` and `fps_den > 0`, and [`Rational::is_valid`]);
    /// - every cell rectangle is finite, lies within `0.0..=1.0` on both axes
    ///   (`x + w <= 1.0`, `y + h <= 1.0`), and has positive extent (`w > 0`,
    ///   `h > 0`);
    /// - every cell's [`opacity`](Cell::opacity) is finite and within the
    ///   closed straight-alpha interval `0.0..=1.0`.
    ///
    /// Cells **may** overlap — that is how picture-in-picture and stacked
    /// layers are expressed — so overlap is intentionally not rejected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] with a precise message identifying the
    /// offending field (or the zero-based index of the offending cell) when any
    /// invariant above is violated.
    pub fn validate(&self) -> Result<()> {
        self.canvas.validate()?;
        for (index, cell) in self.cells.iter().enumerate() {
            validate_cell(index, cell)?;
        }
        Ok(())
    }
}

/// Validate a single cell rectangle, attributing failures to `index`.
fn validate_cell(index: usize, cell: &Cell) -> Result<()> {
    let fields = [("x", cell.x), ("y", cell.y), ("w", cell.w), ("h", cell.h)];
    for (name, value) in fields {
        if !value.is_finite() {
            return Err(Error::Config(format!(
                "cell {index}: {name} must be finite (got {value})"
            )));
        }
    }
    if cell.w <= 0.0 {
        return Err(Error::Config(format!(
            "cell {index}: w must be > 0 (got {})",
            cell.w
        )));
    }
    if cell.h <= 0.0 {
        return Err(Error::Config(format!(
            "cell {index}: h must be > 0 (got {})",
            cell.h
        )));
    }
    if cell.x < 0.0 || cell.y < 0.0 {
        return Err(Error::Config(format!(
            "cell {index}: origin ({}, {}) must be within 0.0..=1.0",
            cell.x, cell.y
        )));
    }
    if cell.x + cell.w > 1.0 {
        return Err(Error::Config(format!(
            "cell {index}: x + w = {} exceeds 1.0",
            cell.x + cell.w
        )));
    }
    if cell.y + cell.h > 1.0 {
        return Err(Error::Config(format!(
            "cell {index}: y + h = {} exceeds 1.0",
            cell.y + cell.h
        )));
    }
    if !cell.opacity.is_finite() {
        return Err(Error::Config(format!(
            "cell {index}: opacity must be finite (got {})",
            cell.opacity
        )));
    }
    if cell.opacity < 0.0 || cell.opacity > 1.0 {
        return Err(Error::Config(format!(
            "cell {index}: opacity must be within 0.0..=1.0 (got {})",
            cell.opacity
        )));
    }
    Ok(())
}

/// Per-edge bezel compensation for a video wall, in physical pixels.
///
/// Mounted display walls have a physical bezel between adjacent panels;
/// compensating means inserting a matching gap in the rendered image so a
/// horizontal/vertical line stays straight across the seam. Both values are the
/// total pixels hidden behind a bezel between two adjacent heads and must be
/// `>= 0`. The default is no compensation (both zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BezelCompensation {
    /// Horizontal bezel gap, in pixels, between horizontally adjacent heads.
    #[serde(default)]
    pub horizontal_px: i32,
    /// Vertical bezel gap, in pixels, between vertically adjacent heads.
    #[serde(default)]
    pub vertical_px: i32,
}

impl BezelCompensation {
    /// Validate that both compensation values are non-negative.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if either value is negative.
    pub fn validate(&self) -> Result<()> {
        if self.horizontal_px < 0 {
            return Err(Error::Config(format!(
                "bezel horizontal_px must be >= 0 (got {})",
                self.horizontal_px
            )));
        }
        if self.vertical_px < 0 {
            return Err(Error::Config(format!(
                "bezel vertical_px must be >= 0 (got {})",
                self.vertical_px
            )));
        }
        Ok(())
    }
}

/// One output **head**: an independent canvas + orientation rendering a named
/// layout.
///
/// A head is the binding between a physical/virtual output surface (its
/// [`canvas`](Head::canvas) and [`orientation`](Head::orientation)) and the
/// [`layout`](Head::layout) drawn on it. Multiple heads (each with its own
/// resolution and layout) compose a multi-head wall; a head is also valid
/// standalone (a single-output multiviewer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    /// Stable head identifier, unique within a wall.
    pub id: String,
    /// The output canvas for this head.
    pub canvas: Canvas,
    /// Output orientation (landscape/portrait). Defaults to landscape.
    #[serde(default)]
    pub orientation: Orientation,
    /// Name of the [`Layout`] rendered on this head.
    pub layout: String,
}

impl Head {
    /// Validate this head: non-empty id and layout name, and a valid canvas.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] for an empty id, an empty layout name, or an
    /// invalid [`Canvas`] (see [`Canvas::validate`]).
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            return Err(Error::Config("head id must not be empty".to_owned()));
        }
        if self.layout.is_empty() {
            return Err(Error::Config(format!(
                "head {}: layout name must not be empty",
                self.id
            )));
        }
        self.canvas.validate()
    }
}

/// A video wall: a `cols` x `rows` grid of [`Head`]s with bezel compensation.
///
/// The wall spans one logical picture across several physical heads. Validation
/// enforces a positive grid, an exact head count (`cols * rows`), unique head
/// ids, valid heads, and non-negative bezel compensation. A single-head wall
/// (`1 x 1`) is the degenerate case of a standalone output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoWall {
    /// Wall name.
    pub name: String,
    /// Number of head columns (`> 0`).
    pub cols: u32,
    /// Number of head rows (`> 0`).
    pub rows: u32,
    /// Bezel compensation between adjacent heads.
    #[serde(default)]
    pub bezel: BezelCompensation,
    /// The heads, in row-major order; exactly `cols * rows` of them.
    pub heads: Vec<Head>,
}

impl VideoWall {
    /// Validate the wall: positive grid, exact head count, unique ids, valid
    /// heads, and non-negative bezel compensation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] describing the first violation: a zero grid
    /// dimension, a head-count mismatch against `cols * rows`, a duplicate head
    /// id, an invalid head, or negative bezel compensation.
    pub fn validate(&self) -> Result<()> {
        if self.cols == 0 || self.rows == 0 {
            return Err(Error::Config(format!(
                "video wall {}: grid {}x{} must be positive on both axes",
                self.name, self.cols, self.rows
            )));
        }
        let expected = u64::from(self.cols).saturating_mul(u64::from(self.rows));
        let actual = u64::try_from(self.heads.len()).unwrap_or(u64::MAX);
        if actual != expected {
            return Err(Error::Config(format!(
                "video wall {}: expected {expected} heads ({}x{}), got {actual}",
                self.name, self.cols, self.rows
            )));
        }
        self.bezel.validate()?;
        for (index, head) in self.heads.iter().enumerate() {
            head.validate()?;
            // Reject a head id already seen earlier in the list. `.get(..index)`
            // avoids panicking slice indexing; `index` is always in-bounds so the
            // empty slice is the only fallback.
            let earlier = self.heads.get(..index).unwrap_or(&[]);
            if earlier.iter().any(|h| h.id == head.id) {
                return Err(Error::Config(format!(
                    "video wall {}: duplicate head id {:?}",
                    self.name, head.id
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(fps_num: i64, fps_den: i64) -> Canvas {
        Canvas {
            width: 1920,
            height: 1080,
            fps_num,
            fps_den,
        }
    }

    #[test]
    fn canvas_validate_rejects_subnanosecond_cadence() {
        // fps > 1 GHz → sub-nanosecond per-tick period → rejected (inv #3 / ADR-T018).
        for (num, den) in [
            (10_000_000_000_i64, 1_i64), // period rounds to 0 ns
            (1_500_000_000, 1),          // rounded 1 ns but exact < 1 ns
            (1_000_000_001, 1),          // just over 1 GHz
            (2_000_000_000, 1),
        ] {
            let err = canvas(num, den)
                .validate()
                .expect_err("a sub-nanosecond cadence must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("per-tick period is under 1 ns") || msg.contains("1 GHz"),
                "error should name the sub-ns/1 GHz cause, got: {msg}"
            );
        }
    }

    #[test]
    fn canvas_validate_accepts_the_1ghz_boundary_and_real_cadences() {
        // Exactly 1 GHz (period == 1 ns) is the accepted boundary.
        canvas(1_000_000_000, 1)
            .validate()
            .expect("1 GHz (period == 1 ns) is accepted");
        // Real broadcast cadences are unaffected.
        for (num, den) in [(60, 1), (30_000, 1_001), (24_000, 1_001), (25, 1)] {
            canvas(num, den)
                .validate()
                .expect("a real cadence must validate");
        }
        // The pre-existing positive-rational checks still fire.
        assert!(canvas(0, 1).validate().is_err());
        assert!(canvas(60, 0).validate().is_err());
    }
}
