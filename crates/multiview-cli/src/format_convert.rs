//! Per-output format convert (#342, output-routing §12.4 "one convert per
//! (picture, format)").
//!
//! A [`FormatConvert`] brings a picture (the baked Program canvas, or Clean) to
//! an output's [`AuxFormat`] with [`fit_rects`] + [`blit_region`]. It runs on
//! the **encoder worker's** thread (never on the bake consumer, never on the
//! clock), writes into a bounded ring of [`CONVERT_RING_BUFFERS`] reusable
//! buffers, and never blocks: with every buffer still held downstream it
//! repeats its last converted picture.
//!
//! Workers that share a format share one [`ConvertCache`]: a wait-free
//! [`SwapCell`] holding the newest `(tick, source, picture)`. The first worker
//! to reach a tick converts and publishes; the others reuse it. Two workers
//! racing may both convert (each publishes a complete picture); none ever
//! waits. Colour samples are copied, never converted: the result carries the
//! source's tag (the canvas tag).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use multiview_compositor::pipeline::Nv12Image;
use multiview_framestore::SwapCell;

use crate::nv12_blit::blit_region;
use crate::output_fit::{fit_rects, Rect};
use crate::routing_aux::AuxFormat;

#[cfg(feature = "ffmpeg")]
use crate::output_routing::LiveEncoder;
#[cfg(feature = "ffmpeg")]
use crate::pipeline::PipelineError;
#[cfg(feature = "ffmpeg")]
use crate::rendition_dispatch::{MoveGate, ParameterSets};
#[cfg(feature = "ffmpeg")]
use crate::rendition_workers::{BakedTick, ReorderBound, VideoEncodeWorker};
#[cfg(feature = "ffmpeg")]
use multiview_ffmpeg::EncodedPacket;

/// Reusable converted pictures per convert (bounded, like the aux ring).
pub const CONVERT_RING_BUFFERS: usize = 4;

/// One cached converted picture.
#[derive(Debug)]
pub struct CachedConvert {
    /// The tick it was converted for.
    pub tick: u64,
    /// The source picture, weakly: a hit is a hit only for the very picture
    /// of that tick (a `Weak` keeps its address from being reused).
    source: Weak<Nv12Image>,
    /// The converted picture.
    pub frame: Arc<Nv12Image>,
}

/// The shared, wait-free result of one (picture, format) convert per tick.
#[derive(Debug, Default)]
pub struct ConvertCache {
    latest: SwapCell<Option<Arc<CachedConvert>>>,
    converts: AtomicU64,
}

impl ConvertCache {
    /// A fresh, empty cache.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Converts actually performed through this cache (diagnostics, tests).
    #[must_use]
    pub fn converts(&self) -> u64 {
        self.converts.load(Ordering::Relaxed)
    }
}

/// A convert of pictures to one [`AuxFormat`], owned by one worker thread.
#[derive(Debug)]
pub struct FormatConvert {
    format: AuxFormat,
    cache: Arc<ConvertCache>,
    ring: Vec<Arc<Nv12Image>>,
    last: Option<Arc<Nv12Image>>,
}

impl FormatConvert {
    /// A convert to `format` publishing into (and reading from) `cache`.
    #[must_use]
    pub const fn new(format: AuxFormat, cache: Arc<ConvertCache>) -> Self {
        Self {
            format,
            cache,
            ring: Vec::new(),
            last: None,
        }
    }

    /// The target format.
    #[must_use]
    pub const fn format(&self) -> AuxFormat {
        self.format
    }

    /// Buffers this convert has allocated (at most [`CONVERT_RING_BUFFERS`]).
    #[must_use]
    pub fn allocated(&self) -> usize {
        self.ring.len()
    }

    /// `src` (the picture of `tick`) at this format. A picture already at the
    /// format is returned as the same `Arc` (nothing converted or allocated).
    /// `None` only when the picture cannot be drawn at all (degenerate size)
    /// and nothing was converted before.
    pub fn convert(&mut self, tick: u64, src: &Arc<Nv12Image>) -> Option<Arc<Nv12Image>> {
        if src.width() == self.format.width && src.height() == self.format.height {
            return Some(Arc::clone(src));
        }
        if let Some(hit) = self.cache.latest.load() {
            if hit.tick == tick && std::ptr::eq(hit.source.as_ptr(), Arc::as_ptr(src)) {
                return Some(Arc::clone(&hit.frame));
            }
        }
        let Some(frame) = self.draw(src) else {
            return self.last.clone();
        };
        self.cache.converts.fetch_add(1, Ordering::Relaxed);
        self.cache.latest.store(Some(Arc::new(CachedConvert {
            tick,
            source: Arc::downgrade(src),
            frame: Arc::clone(&frame),
        })));
        self.last = Some(Arc::clone(&frame));
        Some(frame)
    }

    fn draw(&mut self, src: &Nv12Image) -> Option<Arc<Nv12Image>> {
        let f = self.format;
        let (crop, dst) = fit_rects(
            src.width(),
            src.height(),
            f.width,
            f.height,
            f.fit.mode,
            f.fit.anchor,
        )?;
        let color = src.color();
        let index = match self.ring.iter_mut().position(|b| {
            Arc::get_mut(b).is_some_and(|img| img.color() == color && img.alpha_plane().is_none())
        }) {
            Some(index) => index,
            None if self.ring.len() < CONVERT_RING_BUFFERS => {
                let image = Nv12Image::solid(f.width, f.height, 16, 128, 128, color).ok()?;
                self.ring.push(Arc::new(image));
                self.ring.len() - 1
            }
            // Every buffer is still held downstream: repeat (never block,
            // never grow).
            None => return None,
        };
        let buffer = self.ring.get_mut(index)?;
        let image = Arc::get_mut(buffer)?;
        let (y_plane, uv_plane) = image.planes_mut();
        if dst != Rect::full(f.width, f.height) {
            y_plane.fill(16);
            uv_plane.fill(128);
        }
        blit_region(src, crop, f.width, y_plane, uv_plane, dst);
        Some(Arc::clone(buffer))
    }
}

#[cfg(feature = "ffmpeg")]
/// A base (Program) rendition worker at a non-canvas format: converts each
/// baked canvas on THIS worker's thread, then encodes it.
pub struct ConvertWorker {
    convert: FormatConvert,
    inner: Box<dyn VideoEncodeWorker>,
}

#[cfg(feature = "ffmpeg")]
impl ConvertWorker {
    /// Wrap `inner` behind `convert`.
    #[must_use]
    pub fn new(convert: FormatConvert, inner: Box<dyn VideoEncodeWorker>) -> Self {
        Self { convert, inner }
    }
}

#[cfg(feature = "ffmpeg")]
fn converted(convert: &mut FormatConvert, item: &BakedTick) -> Result<BakedTick, PipelineError> {
    let canvas = convert
        .convert(item.tick, &item.canvas)
        .ok_or_else(|| PipelineError::Output {
            kind: "format-convert",
            reason: format!(
                "cannot fit a {}x{} picture into {}x{}",
                item.canvas.width(),
                item.canvas.height(),
                convert.format().width,
                convert.format().height
            ),
        })?;
    Ok(BakedTick {
        tick: item.tick,
        canvas,
    })
}

#[cfg(feature = "ffmpeg")]
impl VideoEncodeWorker for ConvertWorker {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        let item = converted(&mut self.convert, &item)?;
        self.inner.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

#[cfg(feature = "ffmpeg")]
/// A live-opened (routed Program/Clean) encoder at a non-canvas format: the
/// convert runs on the key's own worker thread.
pub struct ConvertLive {
    convert: FormatConvert,
    inner: Box<dyn LiveEncoder>,
}

#[cfg(feature = "ffmpeg")]
impl ConvertLive {
    /// Wrap `inner` behind `convert`.
    #[must_use]
    pub fn new(convert: FormatConvert, inner: Box<dyn LiveEncoder>) -> Self {
        Self { convert, inner }
    }
}

#[cfg(feature = "ffmpeg")]
impl VideoEncodeWorker for ConvertLive {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        let item = converted(&mut self.convert, &item)?;
        self.inner.encode(item)
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

#[cfg(feature = "ffmpeg")]
impl LiveEncoder for ConvertLive {
    fn start_at(&mut self, t0: u64) {
        self.inner.start_at(t0);
    }
    fn time_base(&self) -> multiview_core::time::Rational {
        self.inner.time_base()
    }
    fn move_gate(&self) -> MoveGate {
        self.inner.move_gate()
    }
    fn parameter_sets(&self) -> Option<ParameterSets> {
        self.inner.parameter_sets()
    }
}

/// One shared [`ConvertCache`] per format: renditions/keys at the same format
/// convert each picture once.
#[derive(Debug, Default)]
pub struct ConvertCaches {
    caches: Vec<(AuxFormat, Arc<ConvertCache>)>,
}

impl ConvertCaches {
    /// The cache of `format` (created on first use).
    pub fn get(&mut self, format: AuxFormat) -> Arc<ConvertCache> {
        if let Some((_, cache)) = self.caches.iter().find(|(f, _)| *f == format) {
            return Arc::clone(cache);
        }
        let cache = ConvertCache::new();
        self.caches.push((format, Arc::clone(&cache)));
        cache
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_config::{Anchor, OutputFit, OutputFitMode};

    fn picture(w: u32, h: u32) -> Arc<Nv12Image> {
        Arc::new(Nv12Image::color_bars(w, h, CanvasColor::default()).unwrap())
    }

    fn vertical() -> AuxFormat {
        AuxFormat::new(
            108,
            192,
            OutputFit::new(OutputFitMode::Cover, Anchor::Center),
        )
    }

    #[test]
    fn two_renditions_sharing_a_format_convert_once_per_tick() {
        let cache = ConvertCache::new();
        let mut a = FormatConvert::new(vertical(), Arc::clone(&cache));
        let mut b = FormatConvert::new(vertical(), Arc::clone(&cache));
        for tick in 0..10 {
            let src = picture(192, 108);
            let fa = a.convert(tick, &src).unwrap();
            let fb = b.convert(tick, &src).unwrap();
            assert!(
                Arc::ptr_eq(&fa, &fb),
                "the second rendition reuses the convert"
            );
            assert_eq!((fa.width(), fa.height()), (108, 192));
            assert_eq!(fa.color(), src.color(), "colour tag = canvas tag");
        }
        assert_eq!(
            cache.converts(),
            10,
            "one convert per (picture, format) per tick"
        );
        assert_eq!(b.allocated(), 0, "the sharing rendition allocated nothing");
    }

    #[test]
    fn a_different_picture_for_the_same_tick_is_not_a_hit() {
        let cache = ConvertCache::new();
        let mut a = FormatConvert::new(vertical(), Arc::clone(&cache));
        let mut b = FormatConvert::new(vertical(), Arc::clone(&cache));
        let one = picture(192, 108);
        let two = picture(192, 108);
        a.convert(3, &one).unwrap();
        b.convert(3, &two).unwrap();
        assert_eq!(cache.converts(), 2);
    }

    #[test]
    fn identity_passes_the_same_arc_and_allocates_nothing() {
        let cache = ConvertCache::new();
        let mut c = FormatConvert::new(
            AuxFormat::new(192, 108, OutputFit::default()),
            Arc::clone(&cache),
        );
        let src = picture(192, 108);
        let out = c.convert(0, &src).unwrap();
        assert!(Arc::ptr_eq(&out, &src));
        assert_eq!(c.allocated(), 0);
        assert_eq!(cache.converts(), 0);
    }

    #[test]
    fn the_ring_is_bounded_and_repeats_when_every_buffer_is_held() {
        let mut c = FormatConvert::new(vertical(), ConvertCache::new());
        let mut held = Vec::new();
        for tick in 0..20 {
            held.push(c.convert(tick, &picture(192, 108)).unwrap());
        }
        assert_eq!(c.allocated(), CONVERT_RING_BUFFERS, "never grows");
        // With every buffer held (and the cache holding the newest), the
        // convert repeats its last picture instead of waiting.
        let newest = Arc::clone(&held[CONVERT_RING_BUFFERS - 1]);
        for repeated in &held[CONVERT_RING_BUFFERS..] {
            assert!(Arc::ptr_eq(repeated, &newest), "repeat, never wait or grow");
        }
        drop(held);
        let fresh = c.convert(99, &picture(192, 108)).unwrap();
        assert_eq!((fresh.width(), fresh.height()), (108, 192));
    }

    #[test]
    fn cover_output_equals_blit_region_of_the_expected_crop() {
        let src = picture(192, 108);
        let mut c = FormatConvert::new(vertical(), ConvertCache::new());
        let out = c.convert(0, &src).unwrap();
        let (crop, dst) =
            fit_rects(192, 108, 108, 192, OutputFitMode::Cover, Anchor::Center).unwrap();
        let mut y = vec![0u8; 108 * 192];
        let mut uv = vec![0u8; 108 * 192 / 2];
        blit_region(&src, crop, 108, &mut y, &mut uv, dst);
        assert!(out.y_plane().eq(y.as_slice()), "luma parity");
        assert!(out.uv_plane().eq(uv.as_slice()), "chroma parity");
    }
}
