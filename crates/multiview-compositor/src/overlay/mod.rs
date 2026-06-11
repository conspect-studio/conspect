//! Overlay rasterization (feature `overlay`, ADR-0016 / overlay-rendering.md).
//!
//! This module turns the pure overlay *models* from `multiview-overlay` into
//! pixels. **Stage 1 is the text engine only**: it shapes a string with
//! [`cosmic_text`] + [`swash`](https://docs.rs/swash) and manages each unique
//! glyph's straight-coverage bitmap in an [`etagere`](https://docs.rs/etagere)
//! shelf-packed, byte-capped, LRU-evicting atlas — so a clock ticking
//! `12:34:56 → 12:34:57` re-rasterizes nothing already resident (ADR-0016 §5.1,
//! mpv #7615 lesson). Two bundled OFL faces (`JetBrains` Mono for digits, a Noto
//! Sans face for labels) are embedded so metrics are deterministic with **no
//! host-font dependency**.
//!
//! The engine has **two consumers, one rasterizer** (no metric divergence,
//! overlay-rendering.md §4.4):
//!
//! - [`text::TextEngine::prepare_run`] places each glyph into the persistent
//!   atlas and returns atlas-relative quads — the future GPU sub-pass uploads
//!   the dirty atlas region and draws these quads.
//! - [`text::TextEngine::rasterize_run`] returns each glyph's **premultiplied
//!   RGBA** coverage bitmap + destination rect — the CPU reference path blits
//!   these straight into the `Rgba16Float` linear canvas via
//!   [`crate::blend::over`].
//!
//! Alpha is **straight coverage** out of swash and is **premultiplied at upload
//! time** (`rgb * coverage, coverage`) per invariant #8 / ADR-R008 — the
//! opposite of already-premultiplied libass bitmaps (a future stage). Nothing
//! here builds the GPU sub-pass or wires the compositor: it is the
//! `text → atlas → bitmap` engine and its API only.

pub mod atlas;
mod fonts;
pub mod meters;
pub mod subpass;
pub mod text;
pub mod watermark;

/// The GPU overlay compositing sub-pass (feature `overlay` + `wgpu`): one
/// batched compute pass blending overlay primitives premultiplied-source-over
/// the linear canvas. The CPU reference ([`subpass::blend_overlays`]) is the
/// oracle; this is the matching GPU path (naga-validated GPU-free).
#[cfg(feature = "wgpu")]
pub mod gpu_subpass;

/// GPU image-primitive upload bookkeeping (feature `overlay` + `wgpu`): the
/// content-keyed texture cache (upload-once / reuse / bounded LRU evict) and the
/// premultiply-fade + nearest-source-texel seams the `KIND_IMAGE` branch of
/// `overlay.wgsl` consumes. The CPU reference ([`subpass::blend_overlays`]) is
/// the oracle; this is the matching GPU bookkeeping (GPU-free unit-tested).
#[cfg(feature = "wgpu")]
pub mod gpu_image;

pub use meters::{goniometer, histogram, GonioDot, MeterBar, MeterScale};
pub use subpass::{
    apply_overlays_to_nv12, blend_overlays, clock_face, meter_bar, ClockFaceStyle, HandAngles,
    LinearCanvasBuffer, OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect,
};
pub use text::{
    FontFamily, PreparedGlyph, PreparedRun, RasterizedGlyph, RasterizedRun, TextEngine,
};
pub use watermark::push_tile_watermark;
