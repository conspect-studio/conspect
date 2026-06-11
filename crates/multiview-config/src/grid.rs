//! The pure CSS-grid solver.
//!
//! Resolves a [`GridLayout`] (fr / px / % track lists, gaps, and a
//! `grid-template-areas` ASCII map) into one normalized rectangle per named
//! area. This is the deterministic geometry math behind invariant-free
//! placement: it has no I/O, no clock, and no GPU — given the same grid and
//! canvas size it always produces the same rects.
//!
//! Track sizing mirrors CSS Grid for the subset Multiview uses:
//! - fixed `px` and `%` tracks claim their size first (`%` is of the canvas
//!   extent on that axis);
//! - the remaining space, after subtracting all gaps, is shared between the
//!   `fr` tracks in proportion to their flex factors.
//!
//! A named area must occupy a **contiguous rectangle** of grid tracks
//! (`grid-template-areas` does not allow disjoint or L-shaped areas); the
//! solver rejects ragged maps and non-rectangular areas.

use std::str::FromStr;

use crate::error::ConfigError;

/// A single grid track size: a flex factor, a fixed pixel size, or a percentage
/// of the canvas extent on that axis.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Track {
    /// A flex (`fr`) factor; shares leftover space proportionally.
    Fr(f64),
    /// A fixed size in pixels.
    Px(f64),
    /// A percentage (`0.0..=100.0`) of the canvas extent on this axis.
    Percent(f64),
}

impl FromStr for Track {
    type Err = ConfigError;

    /// Parse a CSS-grid track string: `"<n>fr"`, `"<n>px"`, or `"<n>%"`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidTrack`] when the suffix is missing/unknown
    /// or the numeric part does not parse as a finite, non-negative number.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let invalid = || ConfigError::InvalidTrack {
            value: trimmed.to_owned(),
        };
        let parse_num = |raw: &str| -> Result<f64, ConfigError> {
            let value: f64 = raw.trim().parse().map_err(|_| invalid())?;
            if value.is_finite() && value >= 0.0 {
                Ok(value)
            } else {
                Err(invalid())
            }
        };
        if let Some(num) = trimmed.strip_suffix("fr") {
            Ok(Self::Fr(parse_num(num)?))
        } else if let Some(num) = trimmed.strip_suffix("px") {
            Ok(Self::Px(parse_num(num)?))
        } else if let Some(num) = trimmed.strip_suffix('%') {
            Ok(Self::Percent(parse_num(num)?))
        } else {
            Err(invalid())
        }
    }
}

/// A grid layout: column/row track lists, gaps, and the area map.
///
/// This is the solver's input. The serde layout type in
/// [`crate::schema`] converts its string track lists into this structure
/// before solving.
#[derive(Debug, Clone, PartialEq)]
pub struct GridLayout {
    /// Column tracks, left to right.
    pub columns: Vec<Track>,
    /// Row tracks, top to bottom.
    pub rows: Vec<Track>,
    /// Uniform gap (pixels) used when neither axis-specific gap is set.
    pub gap: u32,
    /// Row gap override (pixels).
    pub row_gap: Option<u32>,
    /// Column gap override (pixels).
    pub column_gap: Option<u32>,
    /// `grid-template-areas`: one string per row, space-separated track names.
    pub areas: Vec<String>,
}

impl GridLayout {
    /// The effective column gap in pixels (`column_gap` overriding `gap`).
    #[must_use]
    fn effective_column_gap(&self) -> u32 {
        self.column_gap.unwrap_or(self.gap)
    }

    /// The effective row gap in pixels (`row_gap` overriding `gap`).
    #[must_use]
    fn effective_row_gap(&self) -> u32 {
        self.row_gap.unwrap_or(self.gap)
    }
}

/// One solved area rectangle, normalized to `0.0..=1.0` on each axis.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AreaRect {
    /// The area name from the template map.
    pub name: String,
    /// Left edge (fraction of canvas width).
    pub x: f32,
    /// Top edge (fraction of canvas height).
    pub y: f32,
    /// Width (fraction of canvas width).
    pub w: f32,
    /// Height (fraction of canvas height).
    pub h: f32,
}

/// The pixel offset (start) and size of every track on one axis.
///
/// Gaps are already folded into the cumulative `offsets`, so spanning math
/// needs only the offset/size pair.
struct AxisLayout {
    /// `offsets[i]` is the pixel position of track `i`'s leading edge.
    offsets: Vec<f64>,
    /// `sizes[i]` is the pixel extent of track `i`.
    sizes: Vec<f64>,
}

impl AxisLayout {
    /// Distance from the leading edge of track `start` to the trailing edge of
    /// track `end` (inclusive span), counting interior gaps.
    ///
    /// Returns `None` if either index is out of range.
    fn span(&self, start: usize, end: usize) -> Option<(f64, f64)> {
        let offset = *self.offsets.get(start)?;
        let last_offset = *self.offsets.get(end)?;
        let last_size = *self.sizes.get(end)?;
        let extent = (last_offset + last_size) - offset;
        Some((offset, extent))
    }
}

/// Narrow a finite normalized `f64` (in `[0.0, 1.0]`) to `f32`.
///
/// There is no `TryFrom<f64> for f32` in `std`, and the values here are
/// already constrained to the unit interval by construction, so this lossy
/// conversion is exact enough for layout coordinates and cannot overflow.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
// reason: no safe std conversion (no `TryFrom<f64> for f32`) exists; the inputs
// are finite normalized coordinates in [0,1], so the narrowing cannot overflow
// and only loses sub-f32-epsilon precision that is irrelevant for layout.
fn narrow(value: f64) -> f32 {
    value as f32
}

/// Lay out one axis: resolve track sizes and their pixel offsets.
fn lay_out_axis(tracks: &[Track], extent_px: u32, gap_px: u32) -> Result<AxisLayout, ConfigError> {
    if tracks.is_empty() {
        return Err(ConfigError::Grid("track list must not be empty".to_owned()));
    }
    let extent = f64::from(extent_px);
    let gap = f64::from(gap_px);
    let track_count =
        u32::try_from(tracks.len()).map_err(|_| ConfigError::Grid("too many tracks".to_owned()))?;
    let gap_count = f64::from(track_count.saturating_sub(1));
    let total_gap = gap * gap_count;

    // First pass: fixed (px) and percentage tracks consume space directly;
    // accumulate the total fr flex factor.
    let mut fixed_total = 0.0_f64;
    let mut fr_total = 0.0_f64;
    for track in tracks {
        match *track {
            Track::Px(px) => fixed_total += px,
            Track::Percent(pct) => fixed_total += extent * (pct / 100.0),
            Track::Fr(fr) => fr_total += fr,
        }
    }

    // Space left for the fr tracks after gaps and fixed tracks (never negative).
    let free = (extent - total_gap - fixed_total).max(0.0);

    // Second pass: assign each track its pixel size.
    let mut sizes = Vec::with_capacity(tracks.len());
    for track in tracks {
        let size = match *track {
            Track::Px(px) => px,
            Track::Percent(pct) => extent * (pct / 100.0),
            Track::Fr(fr) => {
                if fr_total > 0.0 {
                    free * (fr / fr_total)
                } else {
                    0.0
                }
            }
        };
        sizes.push(size);
    }

    // Cumulative offsets, inserting a gap between adjacent tracks.
    let mut offsets = Vec::with_capacity(sizes.len());
    let mut cursor = 0.0_f64;
    for (index, size) in sizes.iter().enumerate() {
        if index > 0 {
            cursor += gap;
        }
        offsets.push(cursor);
        cursor += size;
    }

    Ok(AxisLayout { offsets, sizes })
}

/// The inclusive track-index bounding box of a named area.
struct AreaBox {
    col_start: usize,
    col_end: usize,
    row_start: usize,
    row_end: usize,
    /// Number of cells the name occupies (to verify it fills its bounding box).
    cell_count: usize,
}

/// Parse the `grid-template-areas` map into a per-name bounding box, validating
/// that the map is rectangular and that each area is a contiguous rectangle.
fn parse_area_map(
    areas: &[String],
    columns: usize,
    rows: usize,
) -> Result<Vec<(String, AreaBox)>, ConfigError> {
    if areas.len() != rows {
        return Err(ConfigError::Grid(format!(
            "areas has {} row(s) but the grid declares {rows} row track(s)",
            areas.len()
        )));
    }

    // boxes preserves first-seen order so solved rects are deterministic.
    let mut boxes: Vec<(String, AreaBox)> = Vec::new();
    for (row_index, row) in areas.iter().enumerate() {
        let tokens: Vec<&str> = row.split_whitespace().collect();
        if tokens.len() != columns {
            return Err(ConfigError::Grid(format!(
                "areas row {row_index} has {} cell(s) but the grid declares {columns} column track(s)",
                tokens.len()
            )));
        }
        for (col_index, name) in tokens.into_iter().enumerate() {
            if let Some((_, bbox)) = boxes.iter_mut().find(|(existing, _)| existing == name) {
                bbox.col_start = bbox.col_start.min(col_index);
                bbox.col_end = bbox.col_end.max(col_index);
                bbox.row_start = bbox.row_start.min(row_index);
                bbox.row_end = bbox.row_end.max(row_index);
                bbox.cell_count += 1;
            } else {
                boxes.push((
                    name.to_owned(),
                    AreaBox {
                        col_start: col_index,
                        col_end: col_index,
                        row_start: row_index,
                        row_end: row_index,
                        cell_count: 1,
                    },
                ));
            }
        }
    }

    // Each area must fill its own bounding box exactly (i.e. be a rectangle).
    for (name, bbox) in &boxes {
        let cols = bbox.col_end - bbox.col_start + 1;
        let rows_spanned = bbox.row_end - bbox.row_start + 1;
        let expected = cols.saturating_mul(rows_spanned);
        if bbox.cell_count != expected {
            return Err(ConfigError::Grid(format!(
                "area {name:?} is not a contiguous rectangle ({} cells, bounding box holds {expected})",
                bbox.cell_count
            )));
        }
    }

    Ok(boxes)
}

/// Solve a grid into one normalized [`AreaRect`] per named area.
///
/// `canvas_width`/`canvas_height` are the canvas extents in pixels; the
/// returned rectangles are normalized to `0.0..=1.0`.
///
/// # Errors
///
/// Returns [`ConfigError::Grid`] when the canvas has zero extent, a track list
/// is empty, the area map is ragged, or an area is not a contiguous rectangle.
pub fn solve(
    grid: &GridLayout,
    canvas_width: u32,
    canvas_height: u32,
) -> Result<Vec<AreaRect>, ConfigError> {
    if canvas_width == 0 || canvas_height == 0 {
        return Err(ConfigError::Grid("canvas extent must be > 0".to_owned()));
    }

    let cols = lay_out_axis(&grid.columns, canvas_width, grid.effective_column_gap())?;
    let rows = lay_out_axis(&grid.rows, canvas_height, grid.effective_row_gap())?;

    let boxes = parse_area_map(&grid.areas, grid.columns.len(), grid.rows.len())?;

    let width = f64::from(canvas_width);
    let height = f64::from(canvas_height);

    let mut rects = Vec::with_capacity(boxes.len());
    for (name, bbox) in boxes {
        let (x_px, w_px) = cols.span(bbox.col_start, bbox.col_end).ok_or_else(|| {
            ConfigError::Grid(format!(
                "area {name:?} references a column track out of range"
            ))
        })?;
        let (y_px, h_px) = rows.span(bbox.row_start, bbox.row_end).ok_or_else(|| {
            ConfigError::Grid(format!("area {name:?} references a row track out of range"))
        })?;
        rects.push(AreaRect {
            name,
            x: narrow(x_px / width),
            y: narrow(y_px / height),
            w: narrow(w_px / width),
            h: narrow(h_px / height),
        });
    }

    Ok(rects)
}
