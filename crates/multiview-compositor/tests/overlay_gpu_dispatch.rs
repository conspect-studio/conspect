//! GPU-4b: wiring the overlay-image dispatch into the GPU compositor
//! `composite()` path (feature `overlay` + `wgpu`).
//!
//! There is no GPU adapter in CI, so the actual blit + the GPU-vs-CPU SSIM/PSNR
//! parity check run only on a GPU-tagged self-hosted runner (the `#[ignore]`d
//! test below). These always-run tests assert the **CPU-checkable seams** the
//! dispatch consumes:
//!   * `plan_image_uploads` resolves each Image cue against the texture cache,
//!     reporting the layer + `needs_upload` (so the dispatch uploads ONCE),
//!   * the packed primitive carries the resolved texture-array layer + size,
//!   * the plan signals a dispatch is warranted exactly when the draw list has
//!     primitives the subpass would blend (an Image present ⇒ dispatch),
//!   * an empty draw list warrants no dispatch (the no-overlay path is unchanged).
#![cfg(all(feature = "overlay", feature = "wgpu"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::print_stdout,
    clippy::as_conversions,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use multiview_compositor::overlay::gpu_image::{plan_image_uploads, ImageTextureCache};
use multiview_compositor::overlay::gpu_subpass::PrimitiveKind;
use multiview_compositor::overlay::subpass::{OverlayDrawList, OverlayPrimitive, OverlayRect};

fn image_bitmap(width: u32, height: u32, fill: u8) -> Vec<u8> {
    vec![fill; usize::try_from(width * height).unwrap() * 4]
}

fn one_image_list() -> OverlayDrawList {
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(8, 6, 32, 24),
        src_width: 16,
        src_height: 12,
        rgba: image_bitmap(16, 12, 0x90),
        alpha: 0.8,
    });
    list
}

// ----- the upload plan: which layers, upload-once, what the dispatch packs ---

#[test]
fn an_image_cue_plans_one_upload_onto_a_resolved_layer() {
    let list = one_image_list();
    let mut cache = ImageTextureCache::new(8);
    let plan = plan_image_uploads(&list, &mut cache);

    assert!(
        plan.dispatch(),
        "an Image primitive warrants an overlay dispatch"
    );
    assert_eq!(plan.uploads().len(), 1, "exactly one image cue to upload");

    let upload = &plan.uploads()[0];
    assert!(upload.needs_upload, "the first sight of a cue must upload");
    assert_eq!(upload.src_width, 16);
    assert_eq!(upload.src_height, 12);
    assert_eq!(
        upload.rgba.len(),
        16 * 12 * 4,
        "the premultiplied bytes ride the plan"
    );

    // The packed primitive routes to KIND_IMAGE carrying the resolved layer.
    let packed = &plan.prims()[0];
    assert_eq!(packed.kind_meta[0], PrimitiveKind::Image.as_u32());
    assert_eq!(
        packed.kind_meta[1], upload.layer,
        "packed layer == resolved layer"
    );
    assert_eq!(packed.geom[0], 16.0, "source width rides geom.x");
    assert_eq!(packed.color[3], 0.8, "fade rides color.a");
}

#[test]
fn a_static_cue_uploads_once_then_reuses_across_ticks() {
    let list = one_image_list();
    let mut cache = ImageTextureCache::new(8);

    let first = plan_image_uploads(&list, &mut cache);
    assert!(first.uploads()[0].needs_upload, "tick 1 uploads");
    let layer1 = first.uploads()[0].layer;

    // Same content next tick → the cache reuses the resident layer (no upload).
    let second = plan_image_uploads(&list, &mut cache);
    assert!(
        !second.uploads()[0].needs_upload,
        "a static caption is uploaded ONCE, reused thereafter (upload-once)"
    );
    assert_eq!(second.uploads()[0].layer, layer1, "same layer reused");
    assert_eq!(cache.resident_len(), 1, "no second layer consumed");
}

#[test]
fn an_empty_draw_list_warrants_no_dispatch() {
    let list = OverlayDrawList::new();
    let mut cache = ImageTextureCache::new(8);
    let plan = plan_image_uploads(&list, &mut cache);
    assert!(
        !plan.dispatch(),
        "no primitives ⇒ no overlay pass (the no-overlay GPU path is unchanged)"
    );
    assert!(plan.uploads().is_empty());
    assert!(plan.prims().is_empty());
}

#[test]
fn analytic_primitives_dispatch_but_plan_no_image_upload() {
    // A solid rect warrants the overlay dispatch (the subpass blends it) but
    // needs no image-texture upload — only Image cues touch binding 5.
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(0, 0, 10, 10),
        corner_radius: 2,
        color: multiview_compositor::overlay::subpass::OverlayColor::opaque(1.0, 0.0, 0.0),
    });
    let mut cache = ImageTextureCache::new(8);
    let plan = plan_image_uploads(&list, &mut cache);
    assert!(plan.dispatch(), "a rect still needs the overlay pass");
    assert!(
        plan.uploads().is_empty(),
        "no image cue ⇒ no texture upload"
    );
    assert_eq!(plan.prims().len(), 1, "the rect is packed for the subpass");
    assert_eq!(plan.prims()[0].kind_meta[0], PrimitiveKind::Rect.as_u32());
}

// ----- GPU parity: image blit ≈ CPU blend_image (GPU-tagged runner only) -----

mod gpu_parity {
    use super::*;
    use multiview_compositor::blend::LinearRgba;
    use multiview_compositor::error::Error;
    use multiview_compositor::gpu::GpuCompositor;
    use multiview_compositor::overlay::subpass::apply_overlays_to_nv12_reference;
    use multiview_compositor::pipeline::{
        composite as cpu_composite, CanvasColor, Nv12Image, Tile,
    };
    use multiview_core::color::{
        ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
    };

    fn bt709_limited() -> ColorInfo {
        ColorInfo {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristic::Bt709,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
        }
    }

    /// A flat mid-grey NV12 tile (the program canvas the overlay burns onto).
    fn grey_tile(w: u32, h: u32) -> Nv12Image {
        let wu = w as usize;
        let hu = h as usize;
        let y = vec![128u8; wu * hu];
        let uv = vec![128u8; wu * hu / 2];
        Nv12Image::new(w, h, y, uv, bt709_limited()).expect("grey tile geometry")
    }

    fn psnr_y(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let mut sse = 0.0_f64;
        for (x, y) in a.iter().zip(b.iter()) {
            let d = f64::from(i32::from(*x) - i32::from(*y));
            sse += d * d;
        }
        if sse == 0.0 {
            return f64::INFINITY;
        }
        let mse = sse / (a.len() as f64);
        10.0 * (255.0 * 255.0 / mse).log10()
    }

    fn ssim_y(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let n = a.len() as f64;
        let mean = |s: &[u8]| s.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
        let mu_a = mean(a);
        let mu_b = mean(b);
        let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
        for (x, y) in a.iter().zip(b.iter()) {
            let da = f64::from(*x) - mu_a;
            let db = f64::from(*y) - mu_b;
            va += da * da;
            vb += db * db;
            cov += da * db;
        }
        va /= n;
        vb /= n;
        cov /= n;
        let c1 = (0.01 * 255.0_f64).powi(2);
        let c2 = (0.03 * 255.0_f64).powi(2);
        ((2.0 * mu_a * mu_b + c1) * (2.0 * cov + c2))
            / ((mu_a * mu_a + mu_b * mu_b + c1) * (va + vb + c2))
    }

    /// A premultiplied-RGBA image cue with a diagonal alpha ramp (exercises the
    /// nearest-map + premultiplied fade, not a flat fill).
    fn ramp_cue() -> OverlayPrimitive {
        let (sw, sh) = (24u32, 16u32);
        let mut rgba = vec![0u8; (sw * sh) as usize * 4];
        for j in 0..sh {
            for i in 0..sw {
                let a = (((i + j) as f32 / (sw + sh) as f32) * 255.0) as u8;
                let af = a as f32 / 255.0;
                // Premultiplied orange-ish (rgb scaled by alpha).
                let base = ((j * sw + i) * 4) as usize;
                rgba[base] = (1.0 * af * 255.0) as u8;
                rgba[base + 1] = (0.5 * af * 255.0) as u8;
                rgba[base + 2] = (0.1 * af * 255.0) as u8;
                rgba[base + 3] = a;
            }
        }
        OverlayPrimitive::Image {
            dest: OverlayRect::new(20, 12, 48, 32),
            src_width: sw,
            src_height: sh,
            rgba,
            alpha: 0.9,
        }
    }

    /// REQUIRES a GPU (a GPU-tagged self-hosted runner). `#[ignore]` so the
    /// GPU-free CI default never runs it; `cargo test -- --ignored` on a real
    /// adapter does. The GPU image blit must match the CPU `blend_image`
    /// reference within the SSIM/PSNR floor (never bit-exact).
    #[test]
    #[ignore = "needs a real GPU adapter (GPU-tagged self-hosted runner)"]
    fn gpu_image_blit_matches_cpu_blend_image_parity() {
        let gpu = match GpuCompositor::new(None) {
            Ok(g) => g,
            Err(Error::NoAdapter(r) | Error::DeviceRequest(r)) => {
                panic!("the parity test was run without a GPU adapter: {r}");
            }
            Err(other) => panic!("unexpected GPU init error: {other}"),
        };

        let (cw, ch) = (96u32, 64u32);
        let canvas = CanvasColor::default();
        let tile = grey_tile(cw, ch);
        let tiles = [Tile::placed(&tile, 0, 0, 1.0)];
        let mut list = OverlayDrawList::new();
        list.push(ramp_cue());

        // CPU oracle: composite, then burn the cue via the full-canvas reference
        // (the same `blend_image` math the GPU subpass mirrors).
        let base =
            cpu_composite(cw, ch, canvas, LinearRgba::TRANSPARENT, &tiles).expect("cpu composite");
        let cpu =
            apply_overlays_to_nv12_reference(&base, &list, canvas).expect("cpu overlay reference");

        // GPU: composite + overlay dispatch in one path.
        let got = gpu
            .composite_with_overlays(cw, ch, canvas, LinearRgba::TRANSPARENT, &tiles, &list)
            .expect("gpu composite with overlays");

        assert_eq!(got.width(), cpu.width());
        assert_eq!(got.height(), cpu.height());
        let y_ssim = ssim_y(got.y_plane(), cpu.y_plane());
        let y_psnr = psnr_y(got.y_plane(), cpu.y_plane());
        println!("GPU-image vs CPU blend_image: Y SSIM={y_ssim:.5} PSNR={y_psnr:.2}dB");
        assert!(y_ssim >= 0.98, "Y SSIM {y_ssim:.5} below 0.98 floor");
        assert!(y_psnr >= 38.0, "Y PSNR {y_psnr:.2}dB below 38 dB floor");
    }
}
