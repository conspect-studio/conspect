//! # multiview-compositor
//!
//! The custom compositor: scale + place + per-tile color convert +
//! linear-light blend + overlay compositing. The library target is
//! `multiview_compositor`.
//!
//! This crate owns the **color math of invariant #8** as pure, exact,
//! unit-tested Rust, plus a **CPU reference compositor** that runs the math in
//! the one canonical order (color-management.md §2 / ADR-C003), which must
//! never be reordered:
//!
//! detect 4 axes -> range expand -> YUV->RGB matrix -> linearize (EOTF) ->
//! primaries convert (in linear) -> scale + premultiplied-alpha blend (in
//! linear) -> OETF -> RGB->YUV + range compress -> tag output.
//!
//! The default build is **pure Rust** (no GPU, no native libraries): the CPU
//! reference is the golden-frame oracle (bit-exact on CPU). The GPU/wgpu path
//! lives in the `gpu` module behind the off-by-default `wgpu` feature; it runs the
//! **same** fixed pipeline in WGSL and matches the CPU oracle within an
//! SSIM/PSNR threshold (GPU is never bit-exact). Vendor fast paths
//! (`cuda`/`metal`/`vaapi`) remain out of scope for this module.
//!
//! ## Module map
//!
//! - [`range`] — limited<->full quantization range expand/compress (8-bit).
//! - [`matrix`] — YUV'<->R'G'B' matrices (BT.601/709/2020-NCL).
//! - [`transfer`] — EOTF/OETF (sRGB, BT.709/BT.1886, PQ ST 2084, HLG B67).
//! - [`transfer_lut`] — table-of-values evaluation of the EOTF/OETF for the
//!   real-time compositor (ADR-0022); `transfer` stays the golden oracle.
//! - [`primaries`] — linear-light gamut conversion (709<->2020 via XYZ).
//! - [`blend`] — premultiplied-alpha source-over in linear light.
//! - [`pipeline`] — the fixed-order pipeline + CPU reference compositor.
//! - [`native`] — host-side admission for the vendor native composite fast
//!   paths (`cuda`/`vaapi`/`metal`): the GPU-free decision of whether a native
//!   island can serve a tile set (inv #5 + ADR-0004) or must fall back to wgpu.
//! - `gpu` *(feature `wgpu`)* — the portable GPU compositor + WGSL shaders
//!   (named as a plain code span: the module is absent from the default doc build).
//! - `overlay` *(feature `overlay`)* — the overlay text engine: cosmic-text +
//!   swash shaping/rasterization into an etagere shelf-packed, byte-capped,
//!   LRU-evicting glyph atlas with bundled OFL fonts (ADR-0016 §3.2). Pure-Rust,
//!   GPU-free; the GPU sub-pass and CPU-reference blit consume it.
//! - [`error`] — the per-crate [`Error`] taxonomy.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod blend;
pub mod error;
#[cfg(feature = "wgpu")]
pub mod gpu;
pub mod matrix;
pub mod native;
#[cfg(feature = "overlay")]
pub mod overlay;
pub mod pipeline;
pub mod primaries;
pub mod range;
pub mod transfer;
pub mod transfer_lut;

pub use backend::{normalize_pci_bus_id, AdapterMatchInfo, GpuTarget, RunBackend, RunBackendKind};
pub use error::{Error, Result};
pub use native::{
    admit_native_composite, CompositeBackend, NativeAdmission, NativeRejection, TileFormats,
};
pub use pipeline::{
    canvas_linear_to_output_yuv, composite, composite_with, composite_with_threads,
    tile_yuv_to_canvas_linear, CanvasColor, Nv12Image, Tile,
};
pub use transfer_lut::LutSet;

#[cfg(feature = "wgpu")]
pub use gpu::{GpuCompositor, GpuContext};
