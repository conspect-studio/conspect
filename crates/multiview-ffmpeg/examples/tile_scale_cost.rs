//! Host-side per-frame cost of bringing a decoded picture to a 640×360 tile:
//! the full-size path (1080p/720p NV12 → Lanczos swscale → tile) that runs when
//! NVDEC decodes at source size, versus the identity tile-size copy that remains
//! when NVDEC's fused `-resize` emits the tile directly (inv #6). The GPU-side
//! download shrinks by the same pixel ratio (not measurable without a GPU).
//!
//! Run: `cargo run --release -p multiview-ffmpeg --features ffmpeg --example tile_scale_cost`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::indexing_slicing,
    clippy::items_after_statements
)]

use std::time::Instant;

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;
use multiview_ffmpeg::{ScaleSpec, Scaler};

fn frame(w: u32, h: u32) -> Video {
    let mut f = Video::new(Pixel::NV12, w, h);
    let mut seed = 0x1234_5678_u32;
    for plane in 0..2 {
        for b in f.data_mut(plane).iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = seed.to_be_bytes()[0];
        }
    }
    f
}

/// Tightly pack both planes (what the pipeline's `video_to_nv12` does).
fn pack(f: &Video) -> usize {
    let (w, h) = (
        usize::try_from(f.width()).unwrap(),
        usize::try_from(f.height()).unwrap(),
    );
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for (dst, plane, rows) in [(&mut y, 0, h), (&mut uv, 1, h / 2)] {
        let stride = f.stride(plane);
        for r in 0..rows {
            dst[r * w..(r + 1) * w].copy_from_slice(&f.data(plane)[r * stride..r * stride + w]);
        }
    }
    y.len() + uv.len()
}

fn main() {
    multiview_ffmpeg::ensure_initialized().unwrap();
    let (tw, th) = (640_u32, 360_u32);
    const N: u32 = 300;
    for (sw, sh) in [(1920_u32, 1080_u32), (1280, 720)] {
        let src = frame(sw, sh);
        let mut sws = Scaler::new(
            ScaleSpec::new(Pixel::NV12, sw, sh),
            ScaleSpec::new(Pixel::NV12, tw, th),
        )
        .unwrap();
        let t0 = Instant::now();
        for _ in 0..N {
            let out = sws.run(&src).unwrap();
            std::hint::black_box(pack(&out));
        }
        let full = t0.elapsed().as_secs_f64() * 1e3 / f64::from(N);
        let tile = frame(tw, th);
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(pack(&tile));
        }
        let fused = t0.elapsed().as_secs_f64() * 1e3 / f64::from(N);
        for (label, flags) in [
            (
                "lanczos+accurate",
                ffmpeg::software::scaling::Flags::LANCZOS
                    | ffmpeg::software::scaling::Flags::ACCURATE_RND,
            ),
            ("bicubic", ffmpeg::software::scaling::Flags::BICUBIC),
            ("area", ffmpeg::software::scaling::Flags::AREA),
            ("bilinear", ffmpeg::software::scaling::Flags::BILINEAR),
            (
                "fast_bilinear",
                ffmpeg::software::scaling::Flags::FAST_BILINEAR,
            ),
        ] {
            let mut sws = Scaler::with_flags(
                ScaleSpec::new(Pixel::NV12, sw, sh),
                ScaleSpec::new(Pixel::NV12, tw, th),
                flags,
            )
            .unwrap();
            let t0 = Instant::now();
            for _ in 0..N {
                let out = sws.run(&src).unwrap();
                std::hint::black_box(pack(&out));
            }
            let ms = t0.elapsed().as_secs_f64() * 1e3 / f64::from(N);
            println!(
                "  {sw}x{sh} swscale {label:16} {ms:6.3} ms/frame ({:4.1}% core @29.97)",
                ms * 2.997
            );
        }
        println!(
            "{sw}x{sh} -> {tw}x{th}: source-size decode + swscale {full:6.3} ms/frame | \
             NVDEC fused resize (identity pack) {fused:6.3} ms/frame | \
             at 29.97 fps: {:5.1}% -> {:4.1}% of a core per source",
            full * 29.97 / 10.0,
            fused * 29.97 / 10.0
        );
    }
}
