//! The **multi-head wall composition** model: per-head layout / source / overlay
//! binding plus bezel compensation, as a pure value type (broadcast-multiviewer
//! brief §1 & §7, ADR-MV001).
//!
//! A video wall spans one logical picture across several physical heads, each an
//! independent canvas with its own layout. The structural
//! [`Head`](multiview_core::layout::Head) /
//! [`VideoWall`] / [`BezelCompensation`](multiview_core::layout::BezelCompensation)
//! types live in [`multiview_core::layout`];
//! this module adds the **engine-side binding**: which sources and overlays each
//! head draws, plus the resolved geometry (the per-head pixel rectangle within
//! the wall, with bezel gaps inserted) the compositor drive consumes.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Pure value computation only — no clock, no channel, no I/O. The engine builds
//! a [`WallComposition`] when (re)configuring and reads the per-head bindings on
//! its drive loop; nothing here blocks or `.await`s.
use multiview_core::layout::VideoWall;

use crate::error::{Error, Result};

/// The engine-side binding for one head: the sources and overlays it draws on top
/// of its [`Head`](multiview_core::layout::Head) layout.
///
/// The structural head (id, canvas, orientation, layout name) is referenced by
/// [`head_id`](HeadBinding::head_id); this carries the *dynamic* bindings the
/// control plane mutates at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadBinding {
    /// The structural head id this binding applies to.
    pub head_id: String,
    /// Source ids bound to this head's tiles, in tile order. An entry of `None`
    /// leaves the corresponding tile unbound (placeholder / slate).
    pub sources: Vec<Option<String>>,
    /// Overlay ids drawn on this head, in z-order.
    pub overlays: Vec<String>,
}

impl HeadBinding {
    /// Construct an empty binding for `head_id`.
    #[must_use]
    pub fn new(head_id: impl Into<String>) -> Self {
        Self {
            head_id: head_id.into(),
            sources: Vec::new(),
            overlays: Vec::new(),
        }
    }

    /// Builder: bind `sources` (in tile order).
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<Option<String>>) -> Self {
        self.sources = sources;
        self
    }

    /// Builder: attach `overlays` (in z-order).
    #[must_use]
    pub fn with_overlays(mut self, overlays: Vec<String>) -> Self {
        self.overlays = overlays;
        self
    }
}

/// The resolved pixel placement of one head within the wall canvas.
///
/// Origin is the head's top-left within the **whole-wall** pixel space, with
/// per-seam bezel gaps inserted before the head (so a line drawn across heads
/// stays straight). All values are non-negative pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadPlacement {
    /// Left edge of this head within the wall, in pixels.
    pub x: u32,
    /// Top edge of this head within the wall, in pixels.
    pub y: u32,
    /// Head width in pixels.
    pub width: u32,
    /// Head height in pixels.
    pub height: u32,
}

/// A resolved multi-head wall composition: the wall's heads with their dynamic
/// bindings and their resolved within-wall placements.
///
/// Build with [`WallComposition::resolve`], which validates the underlying
/// [`VideoWall`] and computes each head's bezel-compensated placement in row-major
/// order. The compositor drive reads [`placements`](WallComposition::placements)
/// and [`bindings`](WallComposition::bindings) to draw each head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallComposition {
    wall_name: String,
    cols: u32,
    rows: u32,
    placements: Vec<HeadPlacement>,
    bindings: Vec<HeadBinding>,
    /// Total wall canvas width in pixels (rightmost head edge).
    width: u32,
    /// Total wall canvas height in pixels (bottommost head edge).
    height: u32,
}

impl WallComposition {
    /// Resolve a wall into per-head placements and default (empty) bindings.
    ///
    /// Validates the [`VideoWall`] (positive grid, exact head count, unique ids,
    /// non-negative bezel) and lays the heads out row-major, inserting a bezel gap
    /// before each interior head so the rendered seam matches the physical bezel.
    /// Each head keeps its own canvas dimensions (mixed-resolution heads are
    /// allowed); a column's x-origin is the running sum of prior column widths
    /// plus bezel, and likewise for rows.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidLayout`] if the wall fails validation, or if a
    /// dimension sum overflows `u32`.
    pub fn resolve(wall: &VideoWall) -> Result<Self> {
        wall.validate()
            .map_err(|e| Error::InvalidLayout(e.to_string()))?;
        let placements = place_heads(wall)?;
        let bindings = wall.heads.iter().map(|h| HeadBinding::new(&h.id)).collect();
        let (width, height) = wall_extent(&placements);
        Ok(Self {
            wall_name: wall.name.clone(),
            cols: wall.cols,
            rows: wall.rows,
            placements,
            bindings,
            width,
            height,
        })
    }

    /// The wall name.
    #[must_use]
    pub fn wall_name(&self) -> &str {
        &self.wall_name
    }

    /// The grid dimensions `(cols, rows)`.
    #[must_use]
    pub const fn grid(&self) -> (u32, u32) {
        (self.cols, self.rows)
    }

    /// The total wall canvas size `(width, height)` in pixels, including bezels.
    #[must_use]
    pub const fn extent(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The resolved per-head placements, in row-major order.
    #[must_use]
    pub fn placements(&self) -> &[HeadPlacement] {
        &self.placements
    }

    /// The per-head dynamic bindings, in row-major order.
    #[must_use]
    pub fn bindings(&self) -> &[HeadBinding] {
        &self.bindings
    }

    /// Replace the binding for the head with id `head_id`. Returns `true` if a
    /// matching head was found and updated; `false` (and no change) otherwise.
    pub fn bind_head(&mut self, binding: HeadBinding) -> bool {
        match self
            .bindings
            .iter_mut()
            .find(|b| b.head_id == binding.head_id)
        {
            Some(slot) => {
                *slot = binding;
                true
            }
            None => false,
        }
    }
}

/// Lay out a wall's heads row-major, inserting bezel gaps before interior heads.
fn place_heads(wall: &VideoWall) -> Result<Vec<HeadPlacement>> {
    let bezel = wall.bezel;
    let h_gap = u32::try_from(bezel.horizontal_px.max(0)).unwrap_or(0);
    let v_gap = u32::try_from(bezel.vertical_px.max(0)).unwrap_or(0);

    // Column widths and row heights come from each head's canvas; with mixed
    // resolutions we use the first head of each column/row to size that track.
    let cols = wall.cols;
    let rows = wall.rows;

    // Per-column x-origins (cumulative width + bezel) and per-row y-origins.
    let mut col_x = Vec::with_capacity(usize_of(cols));
    let mut x_cursor = 0_u32;
    for col in 0..cols {
        col_x.push(x_cursor);
        let head = head_at(wall, 0, col)?;
        let gap = if col + 1 < cols { h_gap } else { 0 };
        x_cursor = x_cursor
            .checked_add(head.canvas.width)
            .and_then(|x| x.checked_add(gap))
            .ok_or_else(|| Error::InvalidLayout("wall width overflows u32".to_owned()))?;
    }

    let mut row_y = Vec::with_capacity(usize_of(rows));
    let mut y_cursor = 0_u32;
    for row in 0..rows {
        row_y.push(y_cursor);
        let head = head_at(wall, row, 0)?;
        let gap = if row + 1 < rows { v_gap } else { 0 };
        y_cursor = y_cursor
            .checked_add(head.canvas.height)
            .and_then(|y| y.checked_add(gap))
            .ok_or_else(|| Error::InvalidLayout("wall height overflows u32".to_owned()))?;
    }

    let mut placements = Vec::with_capacity(wall.heads.len());
    for row in 0..rows {
        for col in 0..cols {
            let head = head_at(wall, row, col)?;
            let x = *col_x
                .get(usize_of(col))
                .ok_or_else(|| Error::InvalidLayout("column index out of range".to_owned()))?;
            let y = *row_y
                .get(usize_of(row))
                .ok_or_else(|| Error::InvalidLayout("row index out of range".to_owned()))?;
            placements.push(HeadPlacement {
                x,
                y,
                width: head.canvas.width,
                height: head.canvas.height,
            });
        }
    }
    Ok(placements)
}

/// The wall extent (max right/bottom edge over all placements).
fn wall_extent(placements: &[HeadPlacement]) -> (u32, u32) {
    let mut w = 0_u32;
    let mut h = 0_u32;
    for p in placements {
        w = w.max(p.x.saturating_add(p.width));
        h = h.max(p.y.saturating_add(p.height));
    }
    (w, h)
}

/// Fetch the head at grid position `(row, col)` (row-major), validated to exist.
fn head_at(wall: &VideoWall, row: u32, col: u32) -> Result<&multiview_core::layout::Head> {
    let idx = u64::from(row)
        .checked_mul(u64::from(wall.cols))
        .and_then(|r| r.checked_add(u64::from(col)))
        .ok_or_else(|| Error::InvalidLayout("head index overflow".to_owned()))?;
    let idx = usize::try_from(idx)
        .map_err(|_| Error::InvalidLayout("head index out of range".to_owned()))?;
    wall.heads
        .get(idx)
        .ok_or_else(|| Error::InvalidLayout(format!("missing head at ({row}, {col})")))
}

/// Convert a `u32` grid dimension to `usize` for capacity hints (saturating).
fn usize_of(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}
