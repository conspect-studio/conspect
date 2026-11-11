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
//! - [`EncodeCache`]: a direct-mapped cache of [`ENCODE_CACHE_ENTRIES`]
//!   entries × 16 bytes = 1 MiB each, at most [`ENCODE_CACHE_POOL`] of them
//!   (9 MiB), allocated on first use and reused across frames.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use multiview_core::color::ColorInfo;

use crate::error::Result;
use crate::pipeline::{canvas_linear_to_output_yuv, tile_yuv_to_canvas_linear, CanvasColor};

/// Number of entries of the dense decode table: every `(y8, cb8, cr8)` triple
/// (2^24). Each entry is a `[f32; 3]`, so the table is exactly 192 MiB.
pub const DECODE_TABLE_ENTRIES: usize = 1 << 24;

/// Number of entries in one direct-mapped encode cache (2^16). Each entry is
/// 16 bytes (`[u32; 3]` key bits + `[u8; 3]` value + valid flag), so one cache
/// is 1 MiB.
pub const ENCODE_CACHE_ENTRIES: usize = 1 << 16;

/// Maximum number of encode caches kept alive for reuse across frames: one for
/// the serial bake plus one per band worker (the band fan-out is capped at 8).
/// Worst case `9 × 1 MiB`.
pub const ENCODE_CACHE_POOL: usize = 9;

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

/// One direct-mapped encode-cache entry.
#[derive(Debug, Clone, Copy, Default)]
struct EncodeEntry {
    key: [u32; 3],
    value: [u8; 3],
    valid: bool,
}

/// A bounded, direct-mapped memo of [`canvas_linear_to_output_yuv`] keyed by
/// the exact `f32` bit patterns of its input (so `-0.0`, `+0.0` and every NaN
/// payload are distinct keys). A colliding insert simply evicts the previous
/// occupant of the slot; a lookup only hits on an exact key match, so
/// collisions cost speed, never correctness.
#[derive(Debug)]
pub struct EncodeCache {
    canvas: CanvasColor,
    entries: Box<[EncodeEntry]>,
}

impl EncodeCache {
    /// An empty cache for `canvas` ([`ENCODE_CACHE_ENTRIES`] entries, 1 MiB).
    #[must_use]
    pub fn new(canvas: CanvasColor) -> Self {
        Self {
            canvas,
            entries: vec![EncodeEntry::default(); ENCODE_CACHE_ENTRIES].into_boxed_slice(),
        }
    }

    /// The slot index of a key (a cheap multiplicative hash of the bits).
    #[must_use]
    pub fn slot(key: [u32; 3]) -> usize {
        let h = key[0].wrapping_mul(0x9E37_79B1)
            ^ key[1].wrapping_mul(0x85EB_CA77).rotate_left(11)
            ^ key[2].wrapping_mul(0xC2B2_AE3D).rotate_left(22);
        let h = h ^ (h >> 15);
        usize::try_from(h).unwrap_or(0) & (ENCODE_CACHE_ENTRIES - 1)
    }

    /// Drop every entry and retarget the cache to `canvas`.
    fn reset(&mut self, canvas: CanvasColor) {
        self.canvas = canvas;
        self.entries.fill(EncodeEntry::default());
    }

    /// Whether `lin` (exact bits) is currently cached.
    #[must_use]
    pub fn contains(&self, lin: [f32; 3]) -> bool {
        let key = lin.map(f32::to_bits);
        self.entries
            .get(Self::slot(key))
            .is_some_and(|e| e.valid && e.key == key)
    }

    /// [`canvas_linear_to_output_yuv`]`(lin, canvas)`, served from the cache
    /// on an exact-bits hit, otherwise computed by the oracle and stored.
    ///
    /// # Errors
    ///
    /// Exactly the oracle's errors (errors are never cached).
    pub fn encode(&mut self, lin: [f32; 3], canvas: CanvasColor) -> Result<[u8; 3]> {
        if canvas != self.canvas {
            self.reset(canvas);
        }
        let key = lin.map(f32::to_bits);
        let Some(entry) = self.entries.get_mut(Self::slot(key)) else {
            return canvas_linear_to_output_yuv(lin, canvas);
        };
        if entry.valid && entry.key == key {
            return Ok(entry.value);
        }
        let value = canvas_linear_to_output_yuv(lin, canvas)?;
        *entry = EncodeEntry {
            key,
            value,
            valid: true,
        };
        Ok(value)
    }
}

/// Persistent encode caches, reused across frames. Slots are taken with
/// `try_lock` only — a contended or poisoned slot is skipped (never waited on).
static ENCODE_POOL: [Mutex<Option<Box<EncodeCache>>>; ENCODE_CACHE_POOL] =
    [const { Mutex::new(None) }; ENCODE_CACHE_POOL];

/// Run `f` with a persistent encode cache, preferring pool slot `hint`. Never
/// blocks: if every slot is busy, `f` gets `None` and must use the oracle.
pub(crate) fn with_encode_cache<T>(
    hint: usize,
    canvas: CanvasColor,
    f: impl FnOnce(Option<&mut EncodeCache>) -> T,
) -> T {
    for offset in 0..ENCODE_CACHE_POOL {
        let Some(slot) = ENCODE_POOL.get((hint + offset) % ENCODE_CACHE_POOL) else {
            continue;
        };
        if let Ok(mut guard) = slot.try_lock() {
            let cache = guard.get_or_insert_with(|| Box::new(EncodeCache::new(canvas)));
            return f(Some(cache));
        }
    }
    f(None)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::{EncodeCache, ENCODE_CACHE_ENTRIES};
    use crate::pipeline::{canvas_linear_to_output_yuv, CanvasColor};

    #[test]
    fn colliding_keys_evict_but_never_return_a_wrong_value() {
        let canvas = CanvasColor::default();
        let mut cache = EncodeCache::new(canvas);
        let a = [0.25_f32, 0.5, 0.75];
        let target = EncodeCache::slot(a.map(f32::to_bits));
        // Find a different key that maps to the same slot.
        let b = (1_u32..u32::MAX)
            .map(|i| [f32::from_bits(0x3E80_0000 + i), 0.5, 0.75])
            .find(|k| EncodeCache::slot(k.map(f32::to_bits)) == target)
            .unwrap();
        assert_eq!(
            cache.encode(a, canvas).unwrap(),
            canvas_linear_to_output_yuv(a, canvas).unwrap()
        );
        assert!(cache.contains(a));
        assert_eq!(
            cache.encode(b, canvas).unwrap(),
            canvas_linear_to_output_yuv(b, canvas).unwrap()
        );
        assert!(cache.contains(b));
        assert!(!cache.contains(a), "colliding insert evicts the occupant");
        assert_eq!(
            cache.encode(a, canvas).unwrap(),
            canvas_linear_to_output_yuv(a, canvas).unwrap()
        );
        assert_eq!(cache.entries.len(), ENCODE_CACHE_ENTRIES);
    }

    #[test]
    fn signed_zero_and_nan_payloads_are_distinct_keys() {
        let canvas = CanvasColor::default();
        let mut cache = EncodeCache::new(canvas);
        for lin in [
            [0.0_f32, 0.0, 0.0],
            [-0.0, 0.0, 0.0],
            [f32::NAN, 0.1, 0.1],
            [f32::from_bits(0x7FC0_0001), 0.1, 0.1],
        ] {
            assert_eq!(
                cache.encode(lin, canvas).unwrap(),
                canvas_linear_to_output_yuv(lin, canvas).unwrap()
            );
            assert!(cache.contains(lin));
        }
        // +0.0 and -0.0 are different keys: caching one never serves the other.
        let mut fresh = EncodeCache::new(canvas);
        fresh.encode([0.0, 0.0, 0.0], canvas).unwrap();
        assert!(!fresh.contains([-0.0, 0.0, 0.0]));
    }

    #[test]
    fn a_canvas_change_resets_the_cache() {
        let canvas = CanvasColor::default();
        let mut cache = EncodeCache::new(canvas);
        let lin = [0.3_f32, 0.2, 0.1];
        cache.encode(lin, canvas).unwrap();
        let full = CanvasColor {
            range: multiview_core::color::ColorRange::Full,
            ..canvas
        };
        assert_eq!(
            cache.encode(lin, full).unwrap(),
            canvas_linear_to_output_yuv(lin, full).unwrap()
        );
        assert_eq!(
            cache.encode(lin, canvas).unwrap(),
            canvas_linear_to_output_yuv(lin, canvas).unwrap()
        );
    }
}
