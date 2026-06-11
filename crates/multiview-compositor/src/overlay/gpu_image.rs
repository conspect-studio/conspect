//! GPU image-primitive upload bookkeeping for the overlay sub-pass (feature
//! `overlay` + `wgpu`): the content-keyed texture cache and the CPU-checkable
//! seams (premultiply-fade math + nearest-neighbour source mapping) the GPU
//! `KIND_IMAGE` branch in `overlay.wgsl` relies on.
//!
//! A bitmap-caption / DVB-sub cue (`OverlayPrimitive::Image`) carries an
//! **already-premultiplied** RGBA buffer (the shape `multiview_ffmpeg::caption::
//! CueBitmap` emits). The GPU path uploads each *unique* cue bitmap into one
//! layer of an `Rgba8Unorm` texture-array exactly **once**, then samples it in
//! the shader every frame it is visible — so a static caption that persists for
//! many ticks is uploaded once, not per-frame (invariant #5/#6 spirit, ADR-0016
//! "keyed by content revision/hash, never per frame"; ADR-E005 bounded memory).
//!
//! There is **no GPU adapter at runtime here**, so the cache bookkeeping (which
//! layer a content key maps to, upload-once vs reuse, bounded LRU eviction) is
//! pure-Rust and unit-tested on the CPU; the matching `textureLoad` runs only on
//! a GPU-tagged self-hosted runner and is validated SSIM/PSNR vs the CPU
//! reference [`crate::overlay::subpass::blend_overlays`] (never bit-exact).

use std::collections::HashMap;

use crate::overlay::gpu_subpass::OverlayPrimGpu;
use crate::overlay::subpass::{OverlayDrawList, OverlayPrimitive};

/// Hard cap on distinct image-cue layers held in the GPU image texture-array,
/// sizing it at construction. Bounded by design (data-plane memory is fixed,
/// never per-frame; ADR-E005). A request beyond the cap evicts the
/// least-recently-used layer.
pub const MAX_IMAGE_LAYERS: u32 = 64;

/// A stable content key for an image cue: a 64-bit FNV-1a hash of the source
/// dimensions and the premultiplied RGBA bytes.
///
/// Keying on **content** (not a per-frame pointer/index) is what makes the
/// upload happen once: an unchanged caption hashes identically every tick, so it
/// reuses its resident layer; a changed bitmap hashes differently and gets its
/// own layer. (Two distinct bitmaps could in principle collide on a 64-bit hash;
/// the cache also stores the dimensions and re-uploads on a dimension mismatch,
/// so a collision degrades to a re-upload, never a wrong-size sample.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageKey(u64);

impl ImageKey {
    /// Derive the content key from a cue's source size + premultiplied bytes.
    ///
    /// The dimensions are folded into the hash first so two buffers of equal
    /// bytes but different declared geometry key differently.
    #[must_use]
    pub fn from_bitmap(src_width: u32, src_height: u32, rgba: &[u8]) -> Self {
        // FNV-1a (offset basis / prime), folding width, height, then every byte.
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        let mut fold = |byte: u8| {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        };
        for b in src_width.to_le_bytes() {
            fold(b);
        }
        for b in src_height.to_le_bytes() {
            fold(b);
        }
        for &b in rgba {
            fold(b);
        }
        Self(hash)
    }

    /// The raw 64-bit hash (for tests / diagnostics).
    #[must_use]
    pub fn bits(self) -> u64 {
        self.0
    }
}

/// One resident image layer's bookkeeping: which texture-array layer it occupies,
/// its source dimensions (so a hash collision with a different size forces a
/// re-upload), and its last-touched tick (for LRU eviction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResidentLayer {
    layer: u32,
    src_width: u32,
    src_height: u32,
    last_used: u64,
}

/// The outcome of resolving an image cue against the cache: the texture-array
/// layer it now occupies, and whether the caller must `write_texture` its bytes
/// (a fresh upload) or may reuse the layer already on the GPU (no upload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerSlot {
    /// The texture-array layer the cue's bitmap lives in.
    pub layer: u32,
    /// `true` when this resolution claimed a fresh/evicted layer and the caller
    /// must upload the bytes; `false` when the layer was already resident and the
    /// upload is skipped (the upload-once property).
    pub needs_upload: bool,
}

/// A bounded, content-keyed LRU cache mapping image-cue content keys to
/// texture-array layers, so a static overlay bitmap is uploaded **once** and
/// reused across ticks, with the oldest layer evicted when the array is full.
///
/// This is the GPU-free bookkeeping half of the image upload path. The owning
/// GPU subpass holds the matching `Rgba8Unorm` texture-array of
/// [`ImageTextureCache::capacity`] layers and performs the actual
/// `write_texture` only when [`ImageTextureCache::resolve`] reports
/// [`LayerSlot::needs_upload`].
#[derive(Debug)]
pub struct ImageTextureCache {
    capacity: u32,
    /// A monotonically increasing logical clock; each `resolve` advances it so
    /// the smallest `last_used` is the least-recently-used layer.
    clock: u64,
    resident: HashMap<ImageKey, ResidentLayer>,
    /// Layers not yet ever assigned, handed out before any eviction is needed.
    free: Vec<u32>,
}

impl ImageTextureCache {
    /// A cache backing a texture-array of `capacity` layers (clamped to at least
    /// one and to [`MAX_IMAGE_LAYERS`]).
    #[must_use]
    pub fn new(capacity: u32) -> Self {
        let capacity = capacity.clamp(1, MAX_IMAGE_LAYERS);
        // Hand out the highest-numbered free layer first is irrelevant; build the
        // free list so layer 0 is assigned first (pop from the back).
        let mut free = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));
        let mut layer = capacity;
        while layer > 0 {
            layer -= 1;
            free.push(layer);
        }
        Self {
            capacity,
            clock: 0,
            resident: HashMap::new(),
            free,
        }
    }

    /// The number of texture-array layers this cache (and its texture-array) hold.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The number of currently-resident (occupied) layers.
    #[must_use]
    pub fn resident_len(&self) -> usize {
        self.resident.len()
    }

    /// Resolve an image cue to a texture-array layer, assigning a fresh/evicted
    /// layer (and signalling [`LayerSlot::needs_upload`]) the first time a given
    /// content key + size is seen, and reusing the resident layer (no upload) on
    /// every subsequent tick the same content is visible.
    ///
    /// LRU: each call advances a logical clock and stamps the touched layer, so
    /// when the array is full the least-recently-resolved layer is evicted.
    pub fn resolve(&mut self, key: ImageKey, src_width: u32, src_height: u32) -> LayerSlot {
        self.clock = self.clock.saturating_add(1);
        let now = self.clock;

        // Already resident with the SAME geometry → reuse, no upload.
        if let Some(entry) = self.resident.get_mut(&key) {
            if entry.src_width == src_width && entry.src_height == src_height {
                entry.last_used = now;
                return LayerSlot {
                    layer: entry.layer,
                    needs_upload: false,
                };
            }
            // Same key, different size (a hash collision or an in-place content
            // change keyed identically): keep the layer but re-upload the bytes.
            entry.src_width = src_width;
            entry.src_height = src_height;
            entry.last_used = now;
            return LayerSlot {
                layer: entry.layer,
                needs_upload: true,
            };
        }

        // Not resident: claim a free layer, else evict the LRU.
        let layer = if let Some(layer) = self.free.pop() {
            layer
        } else {
            self.evict_lru()
        };
        self.resident.insert(
            key,
            ResidentLayer {
                layer,
                src_width,
                src_height,
                last_used: now,
            },
        );
        LayerSlot {
            layer,
            needs_upload: true,
        }
    }

    /// Evict and return the layer of the least-recently-used resident entry.
    ///
    /// Only called when `free` is empty, so `resident` is non-empty and a layer
    /// is always found; the `unwrap_or` fallback (layer 0) keeps the hot path
    /// panic-free even in the impossible empty case.
    fn evict_lru(&mut self) -> u32 {
        let victim = self
            .resident
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, entry)| (*key, entry.layer));
        match victim {
            Some((key, layer)) => {
                self.resident.remove(&key);
                layer
            }
            None => 0,
        }
    }
}

/// Apply the uniform layer-opacity `alpha` (`0.0..=1.0`) to one
/// **already-premultiplied** RGBA8 source sample, returning the faded
/// premultiplied unit-float channels `[r, g, b, a]`.
///
/// The bytes are premultiplied (DVB-sub / libass output), so the fade scales the
/// already-premultiplied channels directly — it must **not** premultiply again.
/// This is the exact per-sample math the CPU reference
/// [`crate::overlay::subpass::blend_image`] applies, factored out so the GPU
/// branch and a unit test share one definition (the shader does the same
/// `texel * fade`).
#[must_use]
pub fn premultiply_fade(sample: [u8; 4], alpha: f32) -> [f32; 4] {
    let fade = alpha.clamp(0.0, 1.0);
    [
        unit(sample[0]) * fade,
        unit(sample[1]) * fade,
        unit(sample[2]) * fade,
        unit(sample[3]) * fade,
    ]
}

/// The nearest-neighbour source coordinate for destination index `d` of a
/// `dst_len`-wide destination sampling a `src_len`-wide source:
/// `floor((2*d + 1) * src / (2*dst))`, clamped to `src_len - 1`.
///
/// Identical to the CPU reference `crate::overlay::subpass::nearest` (the same
/// half-pixel-centred map), so the GPU sampled texel matches the CPU blit within
/// SSIM/PSNR. No `as` cast: the arithmetic is `u64` and the result is
/// `try_from`-narrowed with a saturating clamp.
#[must_use]
pub fn nearest_source_texel(d: u32, dst_len: u32, src_len: u32) -> u32 {
    if dst_len == 0 || src_len == 0 {
        return 0;
    }
    let num = (u64::from(d).saturating_mul(2).saturating_add(1)).saturating_mul(u64::from(src_len));
    let den = u64::from(dst_len).saturating_mul(2).max(1);
    let s = num / den;
    let max = u64::from(src_len.saturating_sub(1));
    u32::try_from(s.min(max)).unwrap_or(src_len.saturating_sub(1))
}

/// `u8` `0..=255` as a unit float `0.0..=1.0` (mirrors
/// `crate::overlay::subpass::unit_from_u8`).
fn unit(v: u8) -> f32 {
    f32::from(v) / 255.0
}

/// One image cue's resolved upload: which texture-array `layer` it occupies,
/// whether its premultiplied bytes still need a `write_texture` this tick
/// ([`LayerSlot::needs_upload`] — `false` once it is resident, the upload-once
/// property), and the source bytes + dimensions to upload.
///
/// `rgba` borrows the cue's buffer from the [`OverlayDrawList`] the plan was
/// built from, so no copy is made; the GPU dispatch uploads it directly only
/// when `needs_upload` is set.
#[derive(Debug, Clone, Copy)]
pub struct ImageUpload<'a> {
    /// The texture-array layer this cue's bitmap lives in.
    pub layer: u32,
    /// `true` when the bytes must be uploaded this tick (a fresh/evicted layer);
    /// `false` when the layer is already resident (skip the upload — upload-once).
    pub needs_upload: bool,
    /// Source bitmap width, pixels.
    pub src_width: u32,
    /// Source bitmap height, pixels.
    pub src_height: u32,
    /// The cue's already-premultiplied RGBA bytes, `src_width * src_height * 4`.
    pub rgba: &'a [u8],
}

/// The upload + pack plan for one frame's overlay [`OverlayDrawList`]: the
/// per-Image-cue uploads (resolved against the texture cache, upload-once) and
/// the packed [`OverlayPrimGpu`] list the [`crate::overlay::gpu_subpass::OverlaySubpass`] dispatch consumes.
///
/// This is the GPU-free seam between the cache bookkeeping and the actual
/// dispatch: the GPU compositor calls [`plan_image_uploads`], `write_texture`s
/// each [`ImageUpload`] that `needs_upload`, uploads [`Self::prims`] into the
/// primitive storage buffer, and dispatches the subpass exactly when
/// [`Self::dispatch`] is set. The bookkeeping is fully unit-testable without a
/// GPU; only the `write_texture` + dispatch need an adapter.
#[derive(Debug)]
pub struct ImageUploadPlan<'a> {
    uploads: Vec<ImageUpload<'a>>,
    prims: Vec<OverlayPrimGpu>,
}

impl<'a> ImageUploadPlan<'a> {
    /// The per-Image-cue uploads, in draw order. Empty when the frame has no
    /// image cues (analytic-only overlays touch no image texture).
    #[must_use]
    pub fn uploads(&self) -> &[ImageUpload<'a>] {
        &self.uploads
    }

    /// The packed primitives the overlay subpass blends (images carry their
    /// resolved texture-array layer; analytic primitives are self-describing).
    #[must_use]
    pub fn prims(&self) -> &[OverlayPrimGpu] {
        &self.prims
    }

    /// Whether the overlay subpass should be dispatched this frame: `true` iff
    /// there is at least one primitive to blend. An empty draw list warrants no
    /// dispatch, so the no-overlay GPU composite path stays byte-for-byte
    /// unchanged (composite → encode directly).
    #[must_use]
    pub fn dispatch(&self) -> bool {
        !self.prims.is_empty()
    }
}

/// Resolve a frame's overlay [`OverlayDrawList`] into an [`ImageUploadPlan`]:
/// each [`OverlayPrimitive::Image`] cue is content-keyed and resolved against
/// `cache` (assigning/reusing a texture-array layer, signalling whether its
/// bytes must be uploaded this tick), and every primitive is packed for the
/// [`crate::overlay::gpu_subpass::OverlaySubpass`] dispatch — images onto their resolved layer
/// ([`OverlayPrimGpu::pack_image`]), analytic primitives self-described
/// ([`OverlayPrimGpu::pack`]).
///
/// Glyph primitives are **skipped** here (they sample the persistent glyph
/// atlas, which the GPU compositor's image-only dispatch does not own — the
/// CLI bake owns the atlas; GPU-4b wires the image path). Skipping holds
/// last-good rather than packing a glyph the dispatch cannot sample.
///
/// Packing is bounded at [`crate::overlay::gpu_subpass::MAX_OVERLAY_PRIMS`]
/// (data-plane memory is fixed, never per-frame; ADR-E005).
#[must_use]
pub fn plan_image_uploads<'a>(
    list: &'a OverlayDrawList,
    cache: &mut ImageTextureCache,
) -> ImageUploadPlan<'a> {
    let cap = usize::try_from(crate::overlay::gpu_subpass::MAX_OVERLAY_PRIMS).unwrap_or(usize::MAX);
    let mut uploads = Vec::new();
    let mut prims = Vec::with_capacity(list.primitives.len().min(cap));
    for primitive in &list.primitives {
        if prims.len() >= cap {
            break;
        }
        match primitive {
            OverlayPrimitive::Image {
                src_width,
                src_height,
                rgba,
                ..
            } => {
                let key = ImageKey::from_bitmap(*src_width, *src_height, rgba);
                let slot = cache.resolve(key, *src_width, *src_height);
                uploads.push(ImageUpload {
                    layer: slot.layer,
                    needs_upload: slot.needs_upload,
                    src_width: *src_width,
                    src_height: *src_height,
                    rgba,
                });
                prims.push(OverlayPrimGpu::pack_image(primitive, slot.layer));
            }
            // Glyphs need the persistent atlas this dispatch does not own → skip.
            OverlayPrimitive::Glyph { .. } => {}
            // Analytic primitives (rect / line / stroke / ring) are self-described.
            other => prims.push(OverlayPrimGpu::pack(other, 0, 0)),
        }
    }
    ImageUploadPlan { uploads, prims }
}
