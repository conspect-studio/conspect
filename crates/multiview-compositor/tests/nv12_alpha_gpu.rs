#![cfg(feature = "wgpu")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use multiview_compositor::error::Error;
use multiview_compositor::gpu::compositor::reject_alpha_tiles;
use multiview_compositor::pipeline::{AlphaPlanePool, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

#[test]
fn alpha_tiles_get_typed_gpu_refusal_without_adapter() {
    let color = ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    };
    let opaque = Nv12Image::solid(2, 2, 235, 128, 128, color).unwrap();
    assert_eq!(
        reject_alpha_tiles(&[Tile::placed(&opaque, 0, 0, 1.0)]),
        Ok(())
    );
    let alpha = Arc::new(AlphaPlanePool::default())
        .attach(opaque, &[0, 255, 128, 64])
        .unwrap();
    assert_eq!(
        reject_alpha_tiles(&[Tile::placed(&alpha, 0, 0, 1.0)]),
        Err(Error::GpuAlphaUnsupported)
    );
}
