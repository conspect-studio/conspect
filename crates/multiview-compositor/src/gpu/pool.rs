//! The GPU surface pool: the run-stable textures and buffers the wgpu
//! compositor reuses every tick instead of allocating per frame (EFF-0, safety
//! rule §5 / efficiency-budget §2.4 — "frame buffers come from per-device pools
//! allocated at start, never per-frame").
//!
//! Before this pool, [`crate::gpu::compositor::GpuCompositor`] created the full
//! transient set every `composite()` call — the `Rgba16Float` linear canvas
//! (~16.6 MB at 1080p), the tile texture arrays, the NV12 `y_out`/`uv_out`
//! planes, two padded readback buffers, and the uniform/storage buffers — then
//! freed them via `Drop`. At 4K25 that is ~2.6 GB/s of allocate-then-free GPU
//! churn and a real throughput limiter.
//!
//! The pool holds each of those as a cached field keyed by the dimensions that
//! size it, and **reuses** the cached resource whenever the request fits:
//!
//! - **Canvas-sized surfaces** (`canvas_lin`, `overlaid`, `y_out`, `uv_out`,
//!   the two readback buffers) are keyed on the exact `(canvas_w, canvas_h)`.
//!   The canvas size is run-stable, so a change (a resize) is rare, not
//!   per-tick: on an exact match the cached texture/buffer is returned with **no
//!   allocation**; only a genuine dimension change recreates it.
//! - **Tile arrays** (`y_array` R8, `uv_array` Rg8) are sized to the max tile
//!   dimensions across the request and always [`MAX_TILES`] layers. They grow
//!   monotonically: a request that fits the cached extent reuses it, a larger
//!   one recreates at the new (larger) extent. The shader reads each layer only
//!   over that tile's `src_w x src_h` region (fresh-written every tick via
//!   `write_texture`), so an oversized array is byte-identical — stale texels
//!   outside the written region are never sampled.
//! - **Uniform + storage buffers** (`composite`/`encode`/`overlay` uniforms,
//!   the `MAX_TILES`-sized tile-params storage buffer, the
//!   [`MAX_OVERLAY_PRIMS`]-sized overlay-prim storage buffer, the 1×1 atlas
//!   placeholder) are allocated once at their bounded maximum and refilled in
//!   place with `queue.write_buffer`/`write_texture`; the shaders read only the
//!   `count` entries actually written each tick.
//!
//! Every allocation the pool performs increments [`SurfacePool::alloc_count`],
//! the authoritative counter the EFF-0 allocation-count gate asserts is bounded
//! (one-time setup), not proportional to the tick count.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::gpu::compositor::MAX_TILES;
use crate::gpu::uniforms::{CompositeUniforms, EncodeUniforms, TileParams};

/// Whether a cached resource of `have` extent can serve a request needing
/// `need` — i.e. it is at least as large in every dimension. Pure (no GPU), so
/// the reuse/grow decision is unit-tested without an adapter.
#[must_use]
pub(crate) fn fits(have: Dim3, need: Dim3) -> bool {
    have.width >= need.width && have.height >= need.height && have.layers >= need.layers
}

/// Whether a cached canvas-sized resource keyed on `have` exactly matches the
/// requested `need`. Canvas dimensions are run-stable, so canvas-sized surfaces
/// reuse on an exact match and recreate only on a genuine resize. Pure (no GPU).
#[must_use]
pub(crate) fn exact(have: Dim2, need: Dim2) -> bool {
    have.width == need.width && have.height == need.height
}

/// A 3-D extent (texture-array: width × height × layers). Layers is 1 for a
/// plain 2-D texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dim3 {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Array layer count.
    pub layers: u32,
}

/// A 2-D extent (width × height) keying a canvas-sized resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dim2 {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
}

/// A keyed cache slot: a cached `key` (the dims/size the resource was built for)
/// paired with the GPU resource. The generic reuse core [`ensure_cached`]
/// operates over this so the allocate-vs-reuse + counter logic is one place,
/// unit-tested GPU-free with a dummy resource.
pub(crate) trait Keyed {
    /// The dimension/size key the slot is reused on.
    type Key: Copy;
    /// The cached key.
    fn key(&self) -> Self::Key;
}

/// The shared allocate-vs-reuse core: if `slot` holds a resource whose key
/// `reuse`s for `need`, keep it (no allocation); otherwise build a new one at
/// `build_key` via `make`, bumping `counter` exactly once. This is the single
/// authority for the allocation count the EFF-0 gate asserts is bounded.
pub(crate) fn ensure_cached<'a, T, F, R>(
    slot: &'a mut Option<T>,
    counter: &AtomicU64,
    need: T::Key,
    build_key: T::Key,
    reuse: R,
    make: F,
) -> &'a T
where
    T: Keyed,
    F: FnOnce(T::Key) -> T,
    R: Fn(T::Key, T::Key) -> bool,
{
    let hit = matches!(slot, Some(cached) if reuse(cached.key(), need));
    if hit {
        slot.get_or_insert_with(|| make(build_key))
    } else {
        counter.fetch_add(1, Ordering::Relaxed);
        slot.insert(make(build_key))
    }
}

/// A pooled texture cached with the extent it was allocated for.
#[derive(Debug)]
pub(crate) struct CachedTexture {
    /// The extent the texture was created at.
    pub dim: Dim3,
    /// The texture itself.
    pub texture: wgpu::Texture,
}

impl Keyed for CachedTexture {
    type Key = Dim3;
    fn key(&self) -> Dim3 {
        self.dim
    }
}

/// A pooled buffer cached with the byte size it was allocated for. The padded
/// bytes-per-row that strips the readback row padding is recomputed by the
/// caller from the same geometry (it is a pure function of width × bytes/px),
/// so it is not stored here.
#[derive(Debug)]
pub(crate) struct CachedBuffer {
    /// The size in bytes the buffer was created at (the reuse key).
    pub size: u64,
    /// The buffer itself.
    pub buffer: wgpu::Buffer,
}

impl Keyed for CachedBuffer {
    type Key = u64;
    fn key(&self) -> u64 {
        self.size
    }
}

/// The run-stable GPU resource pool owned by the compositor. Every field is
/// allocated lazily on first use (or first use at a given size) and reused
/// thereafter; nothing here is freed-and-reallocated per tick.
#[derive(Debug, Default)]
pub(crate) struct SurfacePool {
    /// Count of GPU `create_texture` + `create_buffer` calls the pool has made.
    /// The EFF-0 gate asserts this stops growing once the pool is warm. Exposed
    /// `pub(crate)` so the compositor can borrow it disjointly from the surface
    /// slots (destructuring `*pool` yields `&alloc_count` alongside each
    /// `&mut slot` in one statement, which a `self.counter()` accessor — aliasing
    /// the whole pool — could not).
    pub(crate) alloc_count: AtomicU64,

    // --- tile upload arrays (grow-only on dims, always MAX_TILES layers) -----
    /// Tile Y planes (`R8Unorm` texture-array).
    pub y_array: Option<CachedTexture>,
    /// Tile interleaved UV planes (`Rg8Unorm` texture-array, half-res).
    pub uv_array: Option<CachedTexture>,

    // --- canvas-sized surfaces (exact-match reuse, resize is rare) ----------
    /// Linear `Rgba16Float` composite canvas.
    pub canvas_lin: Option<CachedTexture>,
    /// Linear `Rgba16Float` overlaid canvas (overlay path only).
    #[cfg(feature = "overlay")]
    pub overlaid: Option<CachedTexture>,
    /// NV12 `R8Unorm` Y output plane.
    pub y_out: Option<CachedTexture>,
    /// NV12 `Rg8Unorm` UV output plane (half-res).
    pub uv_out: Option<CachedTexture>,
    /// Padded readback buffer for the Y plane.
    pub y_readback: Option<CachedBuffer>,
    /// Padded readback buffer for the UV plane.
    pub uv_readback: Option<CachedBuffer>,

    // --- fixed-size uniform / storage buffers (allocate once, refill) -------
    /// Composite-pass uniform buffer.
    pub comp_uniform: Option<wgpu::Buffer>,
    /// Encode-pass uniform buffer.
    pub enc_uniform: Option<wgpu::Buffer>,
    /// Tile-params storage buffer, sized for [`MAX_TILES`] tiles.
    pub tile_buf: Option<wgpu::Buffer>,
    /// Overlay-pass uniform buffer.
    #[cfg(feature = "overlay")]
    pub ov_uniform: Option<wgpu::Buffer>,
    /// Overlay-prim storage buffer, sized for [`MAX_OVERLAY_PRIMS`] prims.
    #[cfg(feature = "overlay")]
    pub prim_buf: Option<wgpu::Buffer>,
    /// 1×1 placeholder glyph-atlas texture (the image dispatch never samples it).
    #[cfg(feature = "overlay")]
    pub atlas: Option<wgpu::Texture>,
}

impl SurfacePool {
    /// The number of GPU allocations the pool has made since construction.
    /// Steady-state ticks must not increase this (EFF-0 gate).
    #[must_use]
    pub(crate) fn alloc_count(&self) -> u64 {
        self.alloc_count.load(Ordering::Relaxed)
    }

    /// Record one GPU allocation (a `create_texture` / `create_buffer`). Used
    /// only by the test counter; the slot helpers bump the counter directly
    /// (they hold `&mut Option<..>` borrows, not `&self`).
    #[cfg(test)]
    fn bump(&self) {
        self.alloc_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Reuse-or-(re)create a **grow-only** texture-array slot: keep the cached
    /// texture if it [`fits`] the request, otherwise build one that grows past
    /// the old extent in every dimension via `make` (so a later smaller tick
    /// reuses rather than thrashing). Returns a borrow of the resident texture.
    pub(crate) fn grow_texture<'a, F>(
        slot: &'a mut Option<CachedTexture>,
        counter: &AtomicU64,
        need: Dim3,
        make: F,
    ) -> &'a wgpu::Texture
    where
        F: FnOnce(Dim3) -> wgpu::Texture,
    {
        // Grow to at least the request in every dimension if a realloc is needed.
        let build = Dim3 {
            width: slot
                .as_ref()
                .map_or(need.width, |c| c.dim.width.max(need.width)),
            height: slot
                .as_ref()
                .map_or(need.height, |c| c.dim.height.max(need.height)),
            layers: slot
                .as_ref()
                .map_or(need.layers, |c| c.dim.layers.max(need.layers)),
        };
        &ensure_cached(slot, counter, need, build, fits, |d| CachedTexture {
            dim: d,
            texture: make(d),
        })
        .texture
    }

    /// Reuse-or-recreate an **exact-match** canvas-sized texture slot: keep the
    /// cached texture if its `(width, height)` equals the request, otherwise
    /// build one at the request via `make` (a rare resize). `layers` is 1 for
    /// these plain 2-D surfaces.
    pub(crate) fn exact_texture<'a, F>(
        slot: &'a mut Option<CachedTexture>,
        counter: &AtomicU64,
        need: Dim2,
        make: F,
    ) -> &'a wgpu::Texture
    where
        F: FnOnce(Dim2) -> wgpu::Texture,
    {
        let need3 = Dim3 {
            width: need.width,
            height: need.height,
            layers: 1,
        };
        let reuse = |have: Dim3, want: Dim3| {
            exact(
                Dim2 {
                    width: have.width,
                    height: have.height,
                },
                Dim2 {
                    width: want.width,
                    height: want.height,
                },
            )
        };
        &ensure_cached(slot, counter, need3, need3, reuse, |d| CachedTexture {
            dim: d,
            texture: make(Dim2 {
                width: d.width,
                height: d.height,
            }),
        })
        .texture
    }

    /// Reuse-or-recreate an **exact-size** buffer slot (a readback buffer keyed
    /// on its byte size). Built via `make` on a size change.
    pub(crate) fn exact_buffer<'a, F>(
        slot: &'a mut Option<CachedBuffer>,
        counter: &AtomicU64,
        size: u64,
        make: F,
    ) -> &'a CachedBuffer
    where
        F: FnOnce(u64) -> wgpu::Buffer,
    {
        ensure_cached(
            slot,
            counter,
            size,
            size,
            |have, want| have == want,
            |sz| CachedBuffer {
                size: sz,
                buffer: make(sz),
            },
        )
    }

    /// Reuse-or-create a fixed-size buffer slot allocated once and refilled in
    /// place thereafter (uniform / max-sized storage buffers). Built via `make`
    /// on first use only.
    pub(crate) fn fixed_buffer<'a, F>(
        slot: &'a mut Option<wgpu::Buffer>,
        counter: &AtomicU64,
        make: F,
    ) -> &'a wgpu::Buffer
    where
        F: FnOnce() -> wgpu::Buffer,
    {
        if slot.is_none() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        slot.get_or_insert_with(make)
    }

    /// Reuse-or-create a fixed-size texture slot allocated once (the 1×1 atlas
    /// placeholder). Built via `make` on first use only.
    #[cfg(feature = "overlay")]
    pub(crate) fn fixed_texture<'a, F>(
        slot: &'a mut Option<wgpu::Texture>,
        counter: &AtomicU64,
        make: F,
    ) -> &'a wgpu::Texture
    where
        F: FnOnce() -> wgpu::Texture,
    {
        if slot.is_none() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        slot.get_or_insert_with(make)
    }

    /// Bump the counter for an allocation made outside the slot helpers.
    #[cfg(test)]
    pub(crate) fn bump_for_test(&self) {
        self.bump();
    }
}

/// The byte size of the [`MAX_TILES`]-sized tile-params storage buffer.
#[must_use]
pub(crate) fn tile_buf_size() -> u64 {
    let stride = u64::try_from(core::mem::size_of::<TileParams>()).unwrap_or(0);
    stride.saturating_mul(u64::from(MAX_TILES.max(1)))
}

/// The byte size of the composite-pass uniform buffer.
#[must_use]
pub(crate) fn comp_uniform_size() -> u64 {
    u64::try_from(core::mem::size_of::<CompositeUniforms>()).unwrap_or(0)
}

/// The byte size of the encode-pass uniform buffer.
#[must_use]
pub(crate) fn enc_uniform_size() -> u64 {
    u64::try_from(core::mem::size_of::<EncodeUniforms>()).unwrap_or(0)
}

/// The byte size of the overlay-pass uniform buffer.
#[cfg(feature = "overlay")]
#[must_use]
pub(crate) fn ov_uniform_size() -> u64 {
    u64::try_from(core::mem::size_of::<
        crate::overlay::gpu_subpass::OverlayUniforms,
    >())
    .unwrap_or(0)
}

/// The byte size of the [`MAX_OVERLAY_PRIMS`]-sized overlay-prim storage buffer.
/// The plan is already bounded to `MAX_OVERLAY_PRIMS` prims, so this fixed-size
/// buffer always holds the whole packed list and the shader reads only the
/// `count` entries actually written.
#[cfg(feature = "overlay")]
#[must_use]
pub(crate) fn prim_buf_size() -> u64 {
    let stride = u64::try_from(core::mem::size_of::<
        crate::overlay::gpu_subpass::OverlayPrimGpu,
    >())
    .unwrap_or(0);
    let max = u64::from(crate::overlay::gpu_subpass::MAX_OVERLAY_PRIMS.max(1));
    stride.saturating_mul(max)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fits_reuses_when_cached_is_large_enough() {
        let have = Dim3 {
            width: 128,
            height: 128,
            layers: MAX_TILES,
        };
        // Smaller-or-equal request fits (reuse).
        assert!(fits(
            have,
            Dim3 {
                width: 64,
                height: 64,
                layers: 4
            }
        ));
        assert!(fits(have, have));
    }

    #[test]
    fn fits_grows_when_a_dimension_exceeds_cache() {
        let have = Dim3 {
            width: 128,
            height: 128,
            layers: MAX_TILES,
        };
        // Wider than cached → must grow.
        assert!(!fits(
            have,
            Dim3 {
                width: 256,
                height: 64,
                layers: 1
            }
        ));
        // Taller than cached → must grow.
        assert!(!fits(
            have,
            Dim3 {
                width: 64,
                height: 256,
                layers: 1
            }
        ));
        // More layers than cached → must grow.
        assert!(!fits(
            have,
            Dim3 {
                width: 64,
                height: 64,
                layers: MAX_TILES + 1
            }
        ));
    }

    #[test]
    fn exact_reuses_only_on_identical_canvas_dims() {
        let have = Dim2 {
            width: 1920,
            height: 1080,
        };
        assert!(exact(have, have));
        // A different canvas (resize) must NOT reuse.
        assert!(!exact(
            have,
            Dim2 {
                width: 1280,
                height: 720
            }
        ));
        // Even a larger canvas is a resize for exact-keyed surfaces.
        assert!(!exact(
            have,
            Dim2 {
                width: 3840,
                height: 2160
            }
        ));
    }

    #[test]
    fn fixed_sizes_are_nonzero_and_bounded() {
        assert!(tile_buf_size() > 0, "tile buffer must be sized");
        assert!(
            tile_buf_size() >= comp_uniform_size(),
            "tile params dominate a single uniform"
        );
        assert!(comp_uniform_size() > 0);
        assert!(enc_uniform_size() > 0);
    }

    #[test]
    fn alloc_count_starts_at_zero_and_bumps() {
        let pool = SurfacePool::default();
        assert_eq!(pool.alloc_count(), 0);
        pool.bump_for_test();
        pool.bump_for_test();
        assert_eq!(pool.alloc_count(), 2);
    }

    /// A GPU-free stand-in for a cached resource, so `ensure_cached` (the
    /// allocate-vs-reuse + counter core the real textures/buffers route through)
    /// can be unit-tested end-to-end without a wgpu device. `key` is the reuse
    /// key; `built_at` records the key it was constructed with.
    #[derive(Debug, Clone, Copy)]
    struct DummyRes {
        key: u32,
        built_at: u32,
    }

    impl Keyed for DummyRes {
        type Key = u32;
        fn key(&self) -> u32 {
            self.key
        }
    }

    /// Exact-match reuse predicate for the dummy.
    fn dummy_exact(have: u32, want: u32) -> bool {
        have == want
    }

    /// Grow-only reuse predicate for the dummy (cache serves any equal-or-smaller
    /// request).
    fn dummy_fits(have: u32, want: u32) -> bool {
        have >= want
    }

    #[test]
    fn ensure_cached_allocates_once_then_reuses_at_same_key() {
        let counter = AtomicU64::new(0);
        let mut slot: Option<DummyRes> = None;
        let mut builds = 0_u32;

        // First call at key 100 → one allocation.
        let first = *ensure_cached(&mut slot, &counter, 100, 100, dummy_exact, |k| {
            builds += 1;
            DummyRes {
                key: k,
                built_at: k,
            }
        });
        assert_eq!(first.built_at, 100);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(builds, 1);

        // 50 more calls at the SAME key → ZERO further allocations (reuse).
        for _ in 0..50 {
            let r = *ensure_cached(&mut slot, &counter, 100, 100, dummy_exact, |k| {
                builds += 1;
                DummyRes {
                    key: k,
                    built_at: k,
                }
            });
            assert_eq!(r.built_at, 100);
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "steady-state reuse must not allocate"
        );
        assert_eq!(builds, 1, "make must run exactly once for a stable key");
    }

    #[test]
    fn ensure_cached_reallocates_on_a_key_change_then_reuses_again() {
        let counter = AtomicU64::new(0);
        let mut slot: Option<DummyRes> = None;
        let mk = |k: u32| DummyRes {
            key: k,
            built_at: k,
        };

        // Warm at key A.
        ensure_cached(&mut slot, &counter, 64, 64, dummy_exact, mk);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // A genuine key change (a resize) reallocates exactly once.
        ensure_cached(&mut slot, &counter, 256, 256, dummy_exact, mk);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "a resize allocates once"
        );

        // Steady state at the NEW key reuses again (no more allocations).
        for _ in 0..16 {
            ensure_cached(&mut slot, &counter, 256, 256, dummy_exact, mk);
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "reuse resumes after the resize"
        );
    }

    #[test]
    fn ensure_cached_grow_only_reuses_for_smaller_requests() {
        let counter = AtomicU64::new(0);
        let mut slot: Option<DummyRes> = None;
        let mk = |k: u32| DummyRes {
            key: k,
            built_at: k,
        };

        // Build at 128.
        ensure_cached(&mut slot, &counter, 128, 128, dummy_fits, mk);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Smaller requests fit the grown cache → no allocation.
        for need in [64_u32, 32, 100, 128] {
            ensure_cached(&mut slot, &counter, need, need.max(128), dummy_fits, mk);
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "grow-only cache serves smaller-or-equal requests without reallocating"
        );

        // A larger request grows once.
        ensure_cached(&mut slot, &counter, 256, 256, dummy_fits, mk);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "a larger request grows once"
        );
    }
}
