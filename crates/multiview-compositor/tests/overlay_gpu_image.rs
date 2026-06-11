//! GPU-free seams of the overlay IMAGE-primitive upload path (GPU-4, feature
//! `overlay` + `wgpu`).
//!
//! There is no GPU adapter in CI, so the actual premultiplied-RGBA `textureLoad`
//! blit runs only on a GPU-tagged self-hosted runner (validated SSIM/PSNR vs the
//! CPU reference `blend_image`, never bit-exact). These tests assert the
//! CPU-checkable seams the GPU branch depends on:
//!   * the content-keyed texture cache (upload-once / reuse / bounded LRU evict),
//!   * the already-premultiplied × fade math (matches `blend_image`),
//!   * the nearest-neighbour source-texel mapping (matches `nearest`),
//!   * that an Image primitive routes to the texture branch (`KIND_IMAGE`),
//!     carrying its texture-array layer + source size — not the solid Rect arm.
#![cfg(all(feature = "overlay", feature = "wgpu"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_compositor::overlay::gpu_image::{
    nearest_source_texel, premultiply_fade, ImageKey, ImageTextureCache, MAX_IMAGE_LAYERS,
};
use multiview_compositor::overlay::gpu_subpass::{OverlayPrimGpu, PrimitiveKind};
use multiview_compositor::overlay::subpass::{OverlayPrimitive, OverlayRect};

fn bitmap(width: u32, height: u32, fill: u8) -> Vec<u8> {
    vec![fill; usize::try_from(width * height).unwrap() * 4]
}

// ----- content key --------------------------------------------------------

#[test]
fn identical_bitmaps_hash_to_the_same_key_distinct_ones_differ() {
    let a = bitmap(4, 3, 0x10);
    let same = bitmap(4, 3, 0x10);
    let other = bitmap(4, 3, 0x11);
    assert_eq!(
        ImageKey::from_bitmap(4, 3, &a),
        ImageKey::from_bitmap(4, 3, &same),
        "an unchanged caption hashes identically every tick (upload-once)"
    );
    assert_ne!(
        ImageKey::from_bitmap(4, 3, &a),
        ImageKey::from_bitmap(4, 3, &other),
        "a changed bitmap hashes differently (forces a new layer)"
    );
}

#[test]
fn dimensions_are_folded_into_the_key() {
    let bytes = bitmap(2, 6, 0x20); // 48 bytes either way
    assert_ne!(
        ImageKey::from_bitmap(2, 6, &bytes),
        ImageKey::from_bitmap(6, 2, &bytes),
        "equal bytes but different declared geometry key differently"
    );
}

// ----- cache: upload-once / reuse / evict ---------------------------------

#[test]
fn first_resolution_uploads_repeat_resolution_reuses() {
    let mut cache = ImageTextureCache::new(8);
    let rgba = bitmap(5, 5, 0x33);
    let key = ImageKey::from_bitmap(5, 5, &rgba);

    let first = cache.resolve(key, 5, 5);
    assert!(first.needs_upload, "the first sight of a cue must upload");

    let again = cache.resolve(key, 5, 5);
    assert!(
        !again.needs_upload,
        "a static caption is uploaded ONCE, then reused (no per-frame upload)"
    );
    assert_eq!(again.layer, first.layer, "reuse keeps the same layer");
    assert_eq!(cache.resident_len(), 1, "no second layer was consumed");
}

#[test]
fn distinct_images_take_distinct_layers() {
    let mut cache = ImageTextureCache::new(8);
    let a = bitmap(4, 4, 0x01);
    let b = bitmap(4, 4, 0x02);
    let la = cache.resolve(ImageKey::from_bitmap(4, 4, &a), 4, 4);
    let lb = cache.resolve(ImageKey::from_bitmap(4, 4, &b), 4, 4);
    assert!(la.needs_upload && lb.needs_upload);
    assert_ne!(la.layer, lb.layer, "two cues occupy two layers");
    assert_eq!(cache.resident_len(), 2);
}

#[test]
fn cache_is_bounded_and_evicts_the_least_recently_used() {
    // A 2-layer cache: insert A, B (full), touch A, then C must evict B (the LRU,
    // since A was touched more recently) — bounded memory, never grows.
    let mut cache = ImageTextureCache::new(2);
    let a = bitmap(1, 1, 0xA0);
    let b = bitmap(1, 1, 0xB0);
    let c = bitmap(1, 1, 0xC0);
    let ka = ImageKey::from_bitmap(1, 1, &a);
    let kb = ImageKey::from_bitmap(1, 1, &b);
    let kc = ImageKey::from_bitmap(1, 1, &c);

    let la = cache.resolve(ka, 1, 1).layer;
    let lb = cache.resolve(kb, 1, 1).layer;
    // Touch A so B becomes the least-recently-used.
    let _ = cache.resolve(ka, 1, 1);
    assert_eq!(cache.resident_len(), 2, "still full, never grows past 2");

    let sc = cache.resolve(kc, 1, 1);
    assert!(sc.needs_upload, "C is new → upload");
    assert_eq!(sc.layer, lb, "C reuses B's evicted layer (the LRU)");
    assert_eq!(cache.resident_len(), 2, "bounded at capacity");

    // A survived (most-recently-used); B was evicted, so seeing it again uploads.
    assert!(!cache.resolve(ka, 1, 1).needs_upload, "A was kept");
    let _ = la; // A's layer is whatever it was; only the eviction order matters.
    assert!(
        cache.resolve(kb, 1, 1).needs_upload,
        "B was evicted → must re-upload"
    );
}

#[test]
fn capacity_is_clamped_to_the_bounded_maximum() {
    let cache = ImageTextureCache::new(MAX_IMAGE_LAYERS + 100);
    assert_eq!(
        cache.capacity(),
        MAX_IMAGE_LAYERS,
        "capacity never exceeds the bounded cap (ADR-E005)"
    );
    let zero = ImageTextureCache::new(0);
    assert_eq!(zero.capacity(), 1, "at least one layer");
}

#[test]
fn a_dimension_change_under_the_same_key_re_uploads_in_place() {
    let mut cache = ImageTextureCache::new(4);
    let rgba = bitmap(2, 2, 0x44);
    let key = ImageKey::from_bitmap(2, 2, &rgba);
    let first = cache.resolve(key, 2, 2);
    // Same key but a different declared size must re-upload (collision safety),
    // keeping the same layer rather than sampling a wrong-sized texel.
    let resized = cache.resolve(key, 8, 8);
    assert_eq!(resized.layer, first.layer, "stays on its layer");
    assert!(resized.needs_upload, "a size change forces a fresh upload");
}

// ----- premultiply / fade math (matches blend_image) ----------------------

#[test]
fn fade_scales_already_premultiplied_channels_no_re_premultiply() {
    // A premultiplied half-opaque red sample (r = a = 128/255) at full fade is
    // unchanged; the channels are NOT premultiplied a second time.
    let sample = [128u8, 0, 0, 128];
    let full = premultiply_fade(sample, 1.0);
    assert_eq!(full[0], 128.0 / 255.0, "premultiplied red passes through");
    assert_eq!(full[3], 128.0 / 255.0, "alpha passes through");

    // Half fade scales every premultiplied channel by 0.5 (correct premultiplied
    // layer fade — same as blend_image's channel-wise * fade).
    let half = premultiply_fade(sample, 0.5);
    assert!((half[0] - (128.0 / 255.0) * 0.5).abs() < 1e-6);
    assert!((half[3] - (128.0 / 255.0) * 0.5).abs() < 1e-6);

    // Fade is clamped to [0,1]; a zero fade yields a fully transparent sample.
    let zero = premultiply_fade(sample, 0.0);
    assert_eq!(zero, [0.0, 0.0, 0.0, 0.0]);
}

// ----- geometry: nearest source texel (matches `nearest`) -----------------

#[test]
fn nearest_source_texel_centres_the_sample_and_clamps() {
    // 1:1 mapping is the identity.
    for d in 0..4u32 {
        assert_eq!(nearest_source_texel(d, 4, 4), d);
    }
    // Upscale a 2-wide source across a 4-wide dest: each source texel covers two
    // dest columns, half-pixel-centred.
    assert_eq!(nearest_source_texel(0, 4, 2), 0);
    assert_eq!(nearest_source_texel(1, 4, 2), 0);
    assert_eq!(nearest_source_texel(2, 4, 2), 1);
    assert_eq!(nearest_source_texel(3, 4, 2), 1);
    // Out-of-range / degenerate inputs never index past the source.
    assert_eq!(nearest_source_texel(99, 4, 2), 1, "clamped to src_len - 1");
    assert_eq!(nearest_source_texel(0, 0, 2), 0, "zero dest is safe");
    assert_eq!(nearest_source_texel(0, 4, 0), 0, "zero source is safe");
}

#[test]
fn nearest_source_texel_matches_the_cpu_reference_over_a_sweep() {
    // The GPU mapping must equal the CPU `nearest` bit-for-bit so the sampled
    // texel matches the blit. Re-derive the reference formula here and sweep.
    for dst in 1..=17u32 {
        for src in 1..=13u32 {
            for d in 0..dst {
                let num = (u64::from(d) * 2 + 1) * u64::from(src);
                let den = u64::from(dst) * 2;
                let expect = u32::try_from((num / den).min(u64::from(src - 1))).unwrap();
                assert_eq!(
                    nearest_source_texel(d, dst, src),
                    expect,
                    "d={d} dst={dst} src={src}"
                );
            }
        }
    }
}

// ----- routing: Image -> KIND_IMAGE texture branch, not the solid Rect -----

#[test]
fn packing_an_image_routes_to_the_texture_branch_with_layer_and_size() {
    let prim = OverlayPrimitive::Image {
        dest: OverlayRect::new(20, 30, 64, 48),
        src_width: 16,
        src_height: 12,
        rgba: bitmap(16, 12, 0x55),
        alpha: 0.75,
    };
    // Pack onto texture-array layer 7.
    let packed = OverlayPrimGpu::pack_image(&prim, 7);
    assert_eq!(
        packed.kind_meta[0],
        PrimitiveKind::Image.as_u32(),
        "an Image primitive must route to the GPU texture (KIND_IMAGE) branch, \
         not the analytic solid-Rect branch"
    );
    assert_ne!(
        packed.kind_meta[0],
        PrimitiveKind::Rect.as_u32(),
        "explicitly NOT the solid branch"
    );
    assert_eq!(
        packed.kind_meta[1], 7,
        "the texture-array layer index rides meta"
    );
    assert_eq!(packed.rect, [20, 30, 64, 48], "dest box preserved");
    // Source size rides geom.xy so the shader nearest-maps dest -> source texel.
    assert_eq!(packed.geom[0], 16.0, "source width");
    assert_eq!(packed.geom[1], 12.0, "source height");
    // The fade alpha rides color.a (color rgb unused — the texel supplies color).
    assert_eq!(packed.color[3], 0.75, "fade alpha preserved");
}

#[test]
fn the_general_pack_still_routes_image_to_kind_image() {
    // The generic `pack` entry point (used by pack_list) also tags Image as
    // KIND_IMAGE so a bitmap cue never silently falls into the solid arm.
    let prim = OverlayPrimitive::Image {
        dest: OverlayRect::new(0, 0, 8, 8),
        src_width: 8,
        src_height: 8,
        rgba: bitmap(8, 8, 0x01),
        alpha: 1.0,
    };
    let packed = OverlayPrimGpu::pack(&prim, 0, 0);
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Image.as_u32());
}
