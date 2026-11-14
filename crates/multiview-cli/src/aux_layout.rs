//! A routed layout's aux canvas (ADR-0108 slice 3, design §2.2-§2.4, §7).
//!
//! The `routing-aux` thread composes a stored layout's cells from the SAME
//! tile stores Program reads, at the Program tick's `pts` (`read_at`), into one
//! of four reusable canvas buffers. No decoder is opened and nothing is
//! scaled at decode: decode-once, use-many.
//!
//! **The integer path (§7).** The CPU reference compositor costs about 200 ms
//! per 720p frame and cannot run at cadence; the GPU path is gated on mda001.
//! So a routed layout is composed with pure integer arithmetic that mirrors
//! the reference compositor's geometry exactly:
//!
//! * cell rectangles: the drive's `cell_dst_rect` (floor of the normalised
//!   edges), so every cell covers the same pixels as on Program;
//! * scaling: the reference's nearest-neighbour `floor(d * src / dst)`;
//! * draw order: ascending `z`, stable in cell order;
//! * chroma: one Cb/Cr pair per 2x2 block, taken from the block's
//!   bottom-right pixel (the reference's last-writer rule);
//! * a cell with no picture shows its `on_loss` slate, built once at canvas
//!   size and scaled into the cell, exactly as the drive does;
//! * uncovered pixels take the canvas background, encoded once.
//!
//! Colour samples are copied, never converted: every ingest tile, generator
//! and slate carries the canvas tag (§2.4). The reference compositor's
//! linear-light round trip of the same samples is within one code value, so a
//! routed layout and the same layout on a CPU Program differ by at most one
//! code value per sample, with identical geometry (pinned by the tests below).
//!
//! What the integer path cannot draw honestly, it does not fake:
//! a cell with opacity below 1 is refused when the take is planned (control
//! plane); a tile that carries an alpha plane, or whose colour tag differs
//! from the canvas, is drawn as that cell's slate and counted, never shown
//! opaque or mislabelled (invariant #8).

use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::FailoverSlate;
use multiview_core::color::ColorInfo;
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;

use crate::routing_aux::StoreLookup;

/// Reusable canvas buffers per routed layout; with none free the bus holds.
pub const LAYOUT_RING_BUFFERS: usize = 4;

pub use multiview_control::output_routing::RoutedLayout;

/// The output canvas a layout composes onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutCanvasSpec {
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
    /// The canvas colour (slates are built in it; its output tag is the tag
    /// every drawn tile must carry).
    pub color: CanvasColor,
    /// The background `(Y, Cb, Cr)` code values (the canvas background,
    /// encoded once).
    pub background: [u8; 3],
}

impl LayoutCanvasSpec {
    /// The output tag.
    #[must_use]
    pub fn tag(&self) -> ColorInfo {
        self.color.output_tag()
    }
}

/// One cell's precomputed placement.
#[derive(Debug, Clone)]
struct CellPlan {
    source: Option<Arc<str>>,
    slate: FailoverSlate,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Counters a layout canvas keeps (read by tests and diagnostics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutCounters {
    /// Frames composed.
    pub composed: u64,
    /// Ticks held because every buffer was still in use downstream.
    pub held_no_buffer: u64,
    /// Cells drawn as their slate because the tile had alpha or another tag.
    pub undrawable_cells: u64,
    /// Cells drawn as their slate because the source had no picture.
    pub slate_cells: u64,
}

/// One routed layout's compose state on the aux thread.
pub struct LayoutCanvas {
    layout: Arc<RoutedLayout>,
    cells: Vec<CellPlan>,
    spec: LayoutCanvasSpec,
    ring: Vec<Arc<Nv12Image>>,
    slates: HashMap<FailoverSlate, Arc<Nv12Image>>,
    /// Per-cell column map scratch, reused every tick.
    columns: Vec<u32>,
    /// Per-cell resolved pictures, reused every tick.
    pictures: Vec<Arc<Nv12Image>>,
    /// Per source-bound cell (draw order): the `TileRead` state compose saw
    /// this tick (`NoSignal` when the store is missing). Reused every tick.
    states: Vec<(Arc<str>, SourceState)>,
    /// Per source-bound cell (same order as `states`): the frame compose read
    /// this tick, when [`LayoutCanvas::keep_frames`] is on (the overlay
    /// bake's fault detection reads it; no second store read). Reused.
    frames: Vec<Option<Arc<Nv12Image>>>,
    keep_frames: bool,
    counters: LayoutCounters,
    warned: bool,
}

impl std::fmt::Debug for LayoutCanvas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutCanvas")
            .field("layout", &self.layout.id)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

/// The reference compositor's cell rectangle: the floor of each normalised
/// edge times the canvas extent (`multiview_engine::drive::cell_dst_rect`).
#[must_use]
pub fn cell_rect(cell: &multiview_core::layout::Cell, cw: u32, ch: u32) -> (u32, u32, u32, u32) {
    let edge = |frac: f32, extent: u32| -> u32 {
        if !frac.is_finite() || frac <= 0.0 {
            return 0;
        }
        let value = f64::from(frac) * f64::from(extent);
        if value >= f64::from(extent) {
            return extent;
        }
        // Largest integer <= value, by bisection (no `as` casts).
        let (mut lo, mut hi) = (0u32, extent);
        while lo < hi {
            let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
            if f64::from(mid) <= value {
                lo = mid;
            } else {
                hi = mid.saturating_sub(1);
            }
        }
        lo
    };
    let left = edge(cell.x, cw);
    let top = edge(cell.y, ch);
    let right = edge(cell.x + cell.w, cw);
    let bottom = edge(cell.y + cell.h, ch);
    (
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    )
}

/// The reference compositor's nearest-neighbour axis map.
#[must_use]
pub fn map_axis(d: u32, src: u32, dst: u32) -> u32 {
    if dst == 0 || src == 0 {
        return 0;
    }
    if src == dst {
        return d.min(src - 1);
    }
    let idx = u64::from(d) * u64::from(src) / u64::from(dst);
    u32::try_from(idx.min(u64::from(src - 1))).unwrap_or(src - 1)
}

impl LayoutCanvas {
    /// Build the compose state for `layout` on `spec` (rare: once per take of
    /// a new layout body). `None` when the layout's canvas is not the output
    /// canvas (never composed at a wrong size).
    #[must_use]
    pub fn new(layout: Arc<RoutedLayout>, spec: LayoutCanvasSpec) -> Option<Self> {
        if layout.layout.canvas.width != spec.width || layout.layout.canvas.height != spec.height {
            return None;
        }
        let mut order: Vec<usize> = (0..layout.layout.cells.len()).collect();
        order.sort_by_key(|&i| layout.layout.cells.get(i).map_or(i32::MIN, |c| c.z));
        let cells = order
            .iter()
            .filter_map(|&i| {
                let cell = layout.layout.cells.get(i)?;
                let (x, y, w, h) = cell_rect(cell, spec.width, spec.height);
                Some(CellPlan {
                    source: cell.source.as_deref().map(Arc::from),
                    slate: layout.slates.get(i).copied().unwrap_or_default(),
                    x,
                    y,
                    w,
                    h,
                })
            })
            .collect();
        Some(Self {
            layout,
            cells,
            spec,
            ring: Vec::new(),
            slates: HashMap::new(),
            columns: Vec::new(),
            pictures: Vec::new(),
            states: Vec::new(),
            frames: Vec::new(),
            keep_frames: false,
            counters: LayoutCounters::default(),
            warned: false,
        })
    }

    /// The layout this canvas composes.
    #[must_use]
    pub fn layout(&self) -> &Arc<RoutedLayout> {
        &self.layout
    }

    /// The counters so far.
    #[must_use]
    pub const fn counters(&self) -> LayoutCounters {
        self.counters
    }

    /// Per source-bound cell, the state the last [`LayoutCanvas::compose`]
    /// read from its store (`NoSignal` for a missing store) — what the
    /// layout's state chips show this tick.
    #[must_use]
    pub fn cell_states(&self) -> &[(Arc<str>, SourceState)] {
        &self.states
    }

    /// Keep each source-bound cell's read frame for the tick (one `Arc`
    /// clone per cell) so the overlay bake can probe content faults without
    /// reading the stores again. Off by default (a clean layout pays nothing).
    pub fn keep_frames(&mut self, on: bool) {
        self.keep_frames = on;
        if !on {
            self.frames.clear();
        }
    }

    /// Per source-bound cell (parallel to [`LayoutCanvas::cell_states`]): the
    /// frame the last compose read (`None`: no picture), when
    /// [`LayoutCanvas::keep_frames`] is on; empty otherwise.
    #[must_use]
    pub fn cell_frames(&self) -> &[Option<Arc<Nv12Image>>] {
        &self.frames
    }

    fn slate(&mut self, policy: FailoverSlate) -> Option<Arc<Nv12Image>> {
        if let Some(image) = self.slates.get(&policy) {
            return Some(Arc::clone(image));
        }
        let image = multiview_engine::slate::failover_slate_image(
            policy,
            self.spec.width,
            self.spec.height,
            self.spec.color,
        )
        .ok()?;
        let image = Arc::new(image);
        self.slates.insert(policy, Arc::clone(&image));
        Some(image)
    }

    fn free_buffer(&mut self) -> Option<usize> {
        if let Some(index) = self.ring.iter_mut().position(|b| Arc::get_mut(b).is_some()) {
            return Some(index);
        }
        if self.ring.len() < LAYOUT_RING_BUFFERS {
            let [y, cb, cr] = self.spec.background;
            let image = Nv12Image::solid(
                self.spec.width,
                self.spec.height,
                y,
                cb,
                cr,
                self.spec.tag(),
            )
            .ok()?;
            self.ring.push(Arc::new(image));
            return Some(self.ring.len() - 1);
        }
        None
    }

    /// Compose the layout at `pts` from `stores`. `None` holds the bus (every
    /// buffer is still in use downstream). Never blocks: one `read_at` per
    /// bound cell and integer copies into a reused buffer.
    pub fn compose(&mut self, stores: &StoreLookup, pts: MediaTime) -> Option<Arc<Nv12Image>> {
        // Resolve every cell's picture first (store reads are wait-free).
        let tag = self.spec.tag();
        let mut pictures = std::mem::take(&mut self.pictures);
        pictures.clear();
        self.states.clear();
        self.frames.clear();
        for i in 0..self.cells.len() {
            let Some(cell) = self.cells.get(i) else {
                break;
            };
            let policy = cell.slate;
            let read = cell
                .source
                .as_deref()
                .and_then(|id| stores(id))
                .map(|store| store.read_at(pts));
            if let Some(source) = cell.source.as_ref() {
                let state = read
                    .as_ref()
                    .map_or(SourceState::NoSignal, multiview_framestore::TileRead::state);
                self.states.push((Arc::clone(source), state));
            }
            let tile = read.and_then(|read| read.frame().cloned());
            if self.keep_frames && cell.source.is_some() {
                self.frames.push(tile.clone());
            }
            let picture = match tile {
                Some(tile) if tile.alpha_plane().is_none() && tile.color() == tag => Some(tile),
                Some(_) => {
                    self.counters.undrawable_cells += 1;
                    if !self.warned {
                        self.warned = true;
                        tracing::warn!(
                            layout = %self.layout.id,
                            "routing-aux: a routed layout cell carries alpha or another colour \
                             tag; it shows its slate (the integer path draws opaque, same-tag \
                             tiles only)"
                        );
                    }
                    self.slate(policy)
                }
                None => {
                    self.counters.slate_cells += 1;
                    self.slate(policy)
                }
            };
            let Some(picture) = picture else {
                self.pictures = pictures;
                return None;
            };
            pictures.push(picture);
        }
        let Some(index) = self.free_buffer() else {
            self.counters.held_no_buffer += 1;
            pictures.clear();
            self.pictures = pictures;
            return None;
        };
        let (canvas, background) = ((self.spec.width, self.spec.height), self.spec.background);
        let mut columns = std::mem::take(&mut self.columns);
        if let Some(image) = self.ring.get_mut(index).and_then(Arc::get_mut) {
            let (y_plane, uv_plane) = image.planes_mut();
            y_plane.fill(background[0]);
            let chroma = [background[1], background[2]];
            for pair in uv_plane.chunks_exact_mut(2) {
                pair.copy_from_slice(&chroma);
            }
            for (cell, picture) in self.cells.iter().zip(&pictures) {
                draw_cell(picture, cell, canvas, y_plane, uv_plane, &mut columns);
            }
        }
        self.columns = columns;
        // Release the tiles now (last-good retention stays within the tick).
        pictures.clear();
        self.pictures = pictures;
        self.counters.composed += 1;
        self.ring.get(index).map(Arc::clone)
    }
}

/// Draw `src` scaled (nearest neighbour) into `cell` of the canvas planes,
/// exactly as the reference compositor places a tile:
///
/// * a zero destination extent means the source's own extent
///   (`Tile::eff_dst_w`/`eff_dst_h`);
/// * the rectangle is clipped to the canvas;
/// * luma is per pixel; a 2x2 chroma block takes the chroma of its
///   bottom-right pixel, written by the last (highest) cell covering that
///   pixel. That is the reference's last writer: its per-pixel `write_pixel`
///   writes the block's chroma for all four pixels in raster order.
///
/// All index arithmetic is `usize` from `u32` (no wrap on any canvas size), and
/// every walk is bounded by the plane, so a huge or odd geometry ends the walk
/// instead of looping.
fn draw_cell(
    src: &Nv12Image,
    cell: &CellPlan,
    canvas: (u32, u32),
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    columns: &mut Vec<u32>,
) {
    let to = |v: u32| usize::try_from(v).unwrap_or(usize::MAX);
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 {
        return;
    }
    let dst_w = if cell.w == 0 { sw } else { cell.w };
    let dst_h = if cell.h == 0 { sh } else { cell.h };
    let (cw, ch) = (to(canvas.0), to(canvas.1));
    let (x0, y0) = (to(cell.x), to(cell.y));
    // Clipped extents (pixels of the rect inside the canvas).
    let vis_w = to(dst_w).min(cw.saturating_sub(x0));
    let vis_h = to(dst_h).min(ch.saturating_sub(y0));
    if vis_w == 0 || vis_h == 0 {
        return;
    }
    let (sy, suv) = (src.y_plane(), src.uv_plane());
    let sw_u = to(sw);
    columns.clear();
    columns.extend((0..dst_w).take(vis_w).map(|dx| map_axis(dx, sw, dst_w)));
    // Luma: every visible pixel of the rect.
    for (dy, row_index) in (0..dst_h).take(vis_h).zip(y0..) {
        let src_row = to(map_axis(dy, sh, dst_h)) * sw_u;
        let dst_row = row_index * cw + x0;
        let (Some(dst), Some(row)) = (
            y_plane.get_mut(dst_row..dst_row + vis_w),
            sy.get(src_row..src_row + sw_u),
        ) else {
            continue;
        };
        for (d, &sx) in dst.iter_mut().zip(columns.iter()) {
            if let Some(&v) = row.get(to(sx)) {
                *d = v;
            }
        }
    }
    // Chroma: blocks whose bottom-right pixel `(2bx + 1, 2by + 1)` lies in
    // the visible rect. The first such block is `start / 2`; the last ends
    // at the visible far edge.
    let (end_x, end_y) = (x0 + vis_w, y0 + vis_h);
    for by in (y0 / 2)..ch.div_ceil(2) {
        let py = 2 * by + 1;
        if py >= end_y {
            break;
        }
        if py < y0 {
            continue;
        }
        let dy = u32::try_from(py - y0).unwrap_or(u32::MAX);
        let src_cy = to(map_axis(dy, sh, dst_h) / 2);
        let dst_row = by * cw;
        for bx in (x0 / 2)..cw.div_ceil(2) {
            let px = 2 * bx + 1;
            if px >= end_x {
                break;
            }
            if px < x0 {
                continue;
            }
            let sx = columns.get(px - x0).copied().unwrap_or(0);
            let s = src_cy * sw_u + to(sx / 2) * 2;
            let d = dst_row + bx * 2;
            if let (Some(dst), Some(pair)) = (uv_plane.get_mut(d..d + 2), suv.get(s..s + 2)) {
                dst.copy_from_slice(pair);
            }
        }
    }
}

/// Encode the canvas background through the reference back half once (a
/// 2x2 composite with no tiles), so uncovered pixels match Program's.
///
/// # Errors
/// The reference compositor's error for an unsupported canvas colour.
pub fn background_codes(
    color: CanvasColor,
    background: multiview_compositor::blend::LinearRgba,
) -> Result<[u8; 3], multiview_compositor::error::Error> {
    let image = multiview_compositor::pipeline::composite(2, 2, color, background, &[])?;
    let y = image.y_plane().first().copied().unwrap_or(16);
    let cb = image.uv_plane().first().copied().unwrap_or(128);
    let cr = image.uv_plane().get(1).copied().unwrap_or(128);
    Ok([y, cb, cr])
}

#[cfg(test)]
#[path = "aux_layout_tests.rs"]
mod tests;
