#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite_reference, composite_with_threads,
    tile_yuv_to_canvas_linear, AlphaPlanePool, CanvasColor, Nv12Image, Tile,
};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

fn color() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

#[test]
fn optional_alpha_is_pooled_and_validated() {
    let pool = Arc::new(AlphaPlanePool::default());
    let opaque = Nv12Image::solid(2, 2, 235, 128, 128, color()).unwrap();
    assert!(opaque.alpha_plane().is_none());
    assert!(pool.attach(opaque.clone(), &[0, 128]).is_err());
    let image = pool.attach(opaque, &[0, 64, 128, 255]).unwrap();
    assert_eq!(image.alpha_plane(), Some(&[0, 64, 128, 255][..]));
    let ptr = image.alpha_plane().unwrap().as_ptr();
    drop(image);
    let again = pool
        .attach(
            Nv12Image::solid(2, 2, 235, 128, 128, color()).unwrap(),
            &[255; 4],
        )
        .unwrap();
    assert_eq!(again.alpha_plane().unwrap().as_ptr(), ptr);
}

#[test]
fn straight_alpha_blends_once_after_linearization_for_both_cpu_kernels() {
    let pool = Arc::new(AlphaPlanePool::default());
    let image = pool
        .attach(
            Nv12Image::solid(2, 2, 180, 128, 128, color()).unwrap(),
            &[0, 128, 255, 64],
        )
        .unwrap();
    let canvas = CanvasColor::default();
    let background = LinearRgba {
        r: 0.1,
        g: 0.2,
        b: 0.3,
        a: 1.0,
    };
    let tile = Tile::placed(&image, 0, 0, 0.5);
    let expected_lin = tile_yuv_to_canvas_linear(180, 128, 128, color(), canvas).unwrap();
    let expected_y = [0_u8, 128, 255, 64].map(|a| {
        let alpha = f32::from(a) / 255.0 * 0.5;
        let rgb = [background.r, background.g, background.b];
        canvas_linear_to_output_yuv(
            [
                expected_lin[0] * alpha + rgb[0] * (1.0 - alpha),
                expected_lin[1] * alpha + rgb[1] * (1.0 - alpha),
                expected_lin[2] * alpha + rgb[2] * (1.0 - alpha),
            ],
            canvas,
        )
        .unwrap()[0]
    });
    for actual in [
        composite_reference(2, 2, canvas, background, std::slice::from_ref(&tile), false).unwrap(),
        composite_with_threads(2, 2, canvas, background, &[tile], false, 1).unwrap(),
    ] {
        assert_eq!(actual.y_plane(), expected_y);
    }
}

#[test]
fn attach_with_fills_in_place_reuses_one_plane_and_returns_it_on_error() {
    let pool = Arc::new(AlphaPlanePool::default());
    assert_eq!(pool.retained(), 0);
    let image = pool
        .attach_with(
            Nv12Image::solid(4, 2, 16, 128, 128, color()).unwrap(),
            |p| {
                assert_eq!(p.len(), 8);
                for (i, b) in p.iter_mut().enumerate() {
                    *b = u8::try_from(i).unwrap() * 10;
                }
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        image.alpha_plane(),
        Some(&[0, 10, 20, 30, 40, 50, 60, 70][..])
    );
    let ptr = image.alpha_plane().unwrap().as_ptr();
    drop(image);
    assert_eq!(pool.retained(), 1);
    // A failing fill hands the lease straight back: still one plane, no leak.
    let failed = pool.attach_with(
        Nv12Image::solid(4, 2, 16, 128, 128, color()).unwrap(),
        |_| Err(multiview_compositor::error::Error::Geometry("x".to_owned())),
    );
    assert!(failed.is_err());
    assert_eq!(pool.retained(), 1);
    // Steady state: every frame reuses the same allocation; retention stays 1.
    for frame in 0..8_u8 {
        let image = pool
            .attach_with(
                Nv12Image::solid(4, 2, 16, 128, 128, color()).unwrap(),
                |p| {
                    p.fill(frame);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(image.alpha_plane().unwrap().as_ptr(), ptr);
        assert_eq!(image.alpha_plane().unwrap(), &[frame; 8]);
        assert_eq!(pool.retained(), 0);
        drop(image);
        assert_eq!(pool.retained(), 1);
    }
}
