//! Exact memoisation of the overlay bake's colour round-trip.
//!
//! The region-limited overlay bake ([`super::subpass::apply_overlays_to_nv12`])
//! must stay **byte-identical** to the transcendental oracle
//! ([`super::subpass::apply_overlays_to_nv12_reference`]). Both halves of the
//! colour pipeline it runs per dirty pixel are pure, deterministic functions:
//!
//! - decode, [`tile_yuv_to_canvas_linear`]`(y8, cb8, cr8, tile, canvas)`, has an
//!   8-bit × 3 = 2^24-point domain for a fixed `(tile, canvas)` colour pair;
//! - encode, [`canvas_linear_to_output_yuv`]`(lin, canvas)`, is a function of
//!   the exact bit patterns of three `f32`s for a fixed canvas.
//!
//! So both are memoised **from the oracle itself**: every value served here was
//! produced by calling that exact function with that exact input, so a cache
//! hit returns exactly what a fresh call would (exact by construction — no
//! approximation, no re-derived math, no factorisation argument).
//!
//! # Memory (explicit, bounded)
//!
//! - [`DecodeTable`]: one dense table for **one** `(tile, canvas)` colour pair
//!   per process — [`DECODE_TABLE_ENTRIES`] × 12 bytes = 192 MiB. Built once,
//!   in the background, the first time the bake runs (the bake keeps using the
//!   oracle until it is ready, so no frame ever waits for it), or eagerly via
//!   [`prewarm_overlay_decode_table`]. Any other colour pair uses the oracle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use multiview_core::color::ColorInfo;

use crate::error::Result;
use crate::pipeline::{tile_yuv_to_canvas_linear, CanvasColor};

/// Number of entries of the dense decode table: every `(y8, cb8, cr8)` triple
/// (2^24). Each entry is a `[f32; 3]`, so the table is exactly 192 MiB.
pub const DECODE_TABLE_ENTRIES: usize = 1 << 24;

/// The dense, exact decode table for one `(tile, canvas)` colour pair.
#[derive(Debug)]
pub struct DecodeTable {
    tile: ColorInfo,
    canvas: CanvasColor,
    /// `None` when the oracle rejects this colour pair (unsupported axis):
    /// callers then use the oracle, which reports the same error.
    values: Option<Box<[[f32; 3]]>>,
}

impl DecodeTable {
    /// Index of `(y8, cb8, cr8)`: chroma in the high bits so the four luma
    /// samples sharing one NV12 chroma pair land in one small table span.
    fn index(y8: u8, cb8: u8, cr8: u8) -> usize {
        (usize::from(cb8) << 16) | (usize::from(cr8) << 8) | usize::from(y8)
    }

    /// Build the full table by calling the oracle for every input (exact by
    /// construction), fanned out across `workers` threads.
    fn build(tile: ColorInfo, canvas: CanvasColor, workers: usize) -> Self {
        let mut values = vec![[0.0_f32; 3]; DECODE_TABLE_ENTRIES].into_boxed_slice();
        // Each chunk is a whole number of 256-entry `y8` runs.
        let chunk = DECODE_TABLE_ENTRIES.div_ceil(workers.max(1)).div_ceil(256) * 256;
        let ok = std::thread::scope(|scope| {
            let handles: Vec<_> = values
                .chunks_mut(chunk)
                .enumerate()
                .map(|(part, slots)| {
                    scope.spawn(move || -> Result<()> {
                        let base = part * chunk;
                        for (offset, slot) in slots.iter_mut().enumerate() {
                            let key = base + offset;
                            let [_, cb8, cr8, y8] = u32::try_from(key).unwrap_or(0).to_be_bytes();
                            *slot = tile_yuv_to_canvas_linear(y8, cb8, cr8, tile, canvas)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            handles
                .into_iter()
                .all(|handle| matches!(handle.join(), Ok(Ok(()))))
        });
        Self {
            tile,
            canvas,
            values: ok.then_some(values),
        }
    }

    /// The exact decode of `(y8, cb8, cr8)` if this table covers `(tile,
    /// canvas)`; `None` means "call the oracle".
    #[must_use]
    pub fn get(&self, y8: u8, cb8: u8, cr8: u8) -> Option<[f32; 3]> {
        self.values
            .as_deref()
            .and_then(|values| values.get(Self::index(y8, cb8, cr8)))
            .copied()
    }

    /// Whether this table serves the `(tile, canvas)` colour pair.
    #[must_use]
    pub fn covers(&self, tile: ColorInfo, canvas: CanvasColor) -> bool {
        self.values.is_some() && self.tile == tile && self.canvas == canvas
    }
}

static DECODE_TABLE: OnceLock<DecodeTable> = OnceLock::new();
static DECODE_BUILD_STARTED: AtomicBool = AtomicBool::new(false);

/// Threads used to build the decode table: a few cores, never more than 8.
fn build_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .saturating_sub(2)
        .clamp(1, 8)
}

/// The ready decode table for `(tile, canvas)`, **without ever blocking**.
///
/// The first call starts a background build (on its own thread, never the
/// caller's); until it finishes, or for any other colour pair, this returns
/// `None` and the caller uses the oracle.
#[must_use]
pub fn decode_table(tile: ColorInfo, canvas: CanvasColor) -> Option<&'static DecodeTable> {
    if let Some(table) = DECODE_TABLE.get() {
        return table.covers(tile, canvas).then_some(table);
    }
    if !DECODE_BUILD_STARTED.swap(true, Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("multiview-overlay-decode-table".to_owned())
            .spawn(move || {
                DECODE_TABLE.get_or_init(|| DecodeTable::build(tile, canvas, build_workers()));
            });
        if spawned.is_err() {
            // Allow a later bake to retry; this frame uses the oracle.
            DECODE_BUILD_STARTED.store(false, Ordering::Release);
        }
    }
    None
}

/// Build the overlay decode table for `canvas` (input already in the canvas
/// colour space, as the bake requires) on the **calling** thread, blocking
/// until it is ready. Returns whether the table now serves `canvas`.
///
/// Optional: the bake builds it in the background on first use anyway. Call
/// this at bake-thread startup (never on the output clock) to have the fast
/// path from the first frame. Only one colour pair is tabulated per process.
pub fn prewarm_overlay_decode_table(canvas: CanvasColor) -> bool {
    let tile = canvas.output_tag();
    DECODE_BUILD_STARTED.store(true, Ordering::Release);
    DECODE_TABLE
        .get_or_init(|| DecodeTable::build(tile, canvas, build_workers()))
        .covers(tile, canvas)
}
